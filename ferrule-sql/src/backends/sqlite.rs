use crate::connection::{
    AsyncConnection, BulkInsert, ConnectOptions, ExecutionSummary, ForeignKey, QueryResult,
    StatementResult,
};
use crate::error::SqlError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, TypeHint, Value};
use async_trait::async_trait;
use rusqlite::types::Value as SqliteValue;
use rusqlite::Connection as SqliteConn;

pub struct SqliteConnection {
    conn: std::sync::Arc<std::sync::Mutex<SqliteConn>>,
}

#[async_trait]
impl AsyncConnection for SqliteConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let affected = guard
                .execute(&sql, [])
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            Ok(ExecutionSummary {
                rows_affected: Some(affected as u64),
                command_tag: None,
            })
        })
        .await
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let mut stmt = guard
                .prepare(&sql)
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            let col_names = stmt.column_names();
            if col_names.is_empty() {
                return Err(SqlError::QueryFailed(
                    "Statement does not return rows".to_string(),
                ));
            }
            let columns: Vec<ColumnInfo> = col_names
                .iter()
                .map(|name| ColumnInfo {
                    name: name.to_string(),
                    type_hint: TypeHint::Other,
                    nullable: true,
                })
                .collect();

            let mut rows = Vec::new();
            let mut rows_iter = stmt
                .query([])
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            while let Some(row) = rows_iter
                .next()
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?
            {
                let mut values = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    let val: SqliteValue = row
                        .get(i)
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    values.push(sqlite_to_value(val));
                }
                rows.push(values);
            }

            Ok(QueryResult { columns, rows })
        })
        .await
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
    }

    // execute_multi uses the default impl: tries query(), falls back to execute()

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, SqlError> {
        let statements =
            split_sqlite_statements(sql).map_err(|e| SqlError::QueryFailed(e.to_string()))?;
        let mut results = Vec::with_capacity(statements.len());
        for stmt in statements {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            match self.query(stmt).await {
                Ok(result) => results.push(StatementResult::Query(result)),
                Err(SqlError::QueryFailed(_)) => {
                    let summary = self.execute(stmt).await?;
                    results.push(StatementResult::Summary(summary));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), SqlError> {
        let _ = self.query("SELECT 1").await?;
        Ok(())
    }

    async fn list_tables(&mut self, _schema: Option<&str>) -> Result<Vec<String>, SqlError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let mut stmt = guard
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            let names: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            Ok(names)
        })
        .await
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
    }

    async fn describe_table(
        &mut self,
        _schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, SqlError> {
        let table = table.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let sql = format!("PRAGMA table_info({})", escape_sqlite_identifier(&table));
            let mut stmt = guard
                .prepare(&sql)
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            let col_names = stmt.column_names();
            let columns: Vec<ColumnInfo> = col_names
                .iter()
                .map(|name| ColumnInfo {
                    name: name.to_string(),
                    type_hint: TypeHint::String,
                    nullable: true,
                })
                .collect();
            let mut rows = Vec::new();
            let mut rows_iter = stmt
                .query([])
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            while let Some(row) = rows_iter
                .next()
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?
            {
                let mut values = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    let val: SqliteValue = row
                        .get(i)
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    values.push(sqlite_to_value(val));
                }
                rows.push(values);
            }
            Ok(QueryResult { columns, rows })
        })
        .await
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
    }

    async fn primary_key(
        &mut self,
        _schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<String>, SqlError> {
        let table = table.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            // `PRAGMA table_info(t)` exposes a `pk` column: 0 for
            // non-PK columns, otherwise the 1-based key position.
            let sql = format!("PRAGMA table_info({})", escape_sqlite_identifier(&table));
            let mut stmt = guard
                .prepare(&sql)
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            let mut rows = stmt
                .query([])
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            let mut keyed: Vec<(i64, String)> = Vec::new();
            while let Some(row) = rows
                .next()
                .map_err(|e| SqlError::QueryFailed(e.to_string()))?
            {
                let name: String = row
                    .get("name")
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                let pk: i64 = row
                    .get("pk")
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                if pk > 0 {
                    keyed.push((pk, name));
                }
            }
            keyed.sort_by_key(|(pos, _)| *pos);
            Ok(keyed.into_iter().map(|(_, n)| n).collect())
        })
        .await
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
    }

    async fn list_foreign_keys(
        &mut self,
        _schema: Option<&str>,
    ) -> Result<Vec<ForeignKey>, SqlError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            // SQLite has no schema-wide FK catalog; enumerate tables
            // then call `PRAGMA foreign_key_list(t)` per table.
            let tables: Vec<String> = {
                let mut stmt = guard
                    .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                let names: Result<Vec<String>, _> = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?
                    .collect();
                names.map_err(|e| SqlError::QueryFailed(e.to_string()))?
            };
            let mut out: Vec<ForeignKey> = Vec::new();
            for child_table in tables {
                let sql = format!(
                    "PRAGMA foreign_key_list({})",
                    escape_sqlite_identifier(&child_table)
                );
                let mut stmt = guard
                    .prepare(&sql)
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                let mut rows = stmt
                    .query([])
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                let mut by_id: indexmap::IndexMap<i64, ForeignKey> = indexmap::IndexMap::new();
                while let Some(row) = rows
                    .next()
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?
                {
                    let id: i64 = row
                        .get("id")
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    let parent_table: String = row
                        .get("table")
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    let child_col: String = row
                        .get("from")
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    let parent_col: String = row
                        .get("to")
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    let on_delete: Option<String> = row.get("on_delete").ok();
                    let entry = by_id.entry(id).or_insert_with(|| ForeignKey {
                        child_table: child_table.clone(),
                        child_columns: Vec::new(),
                        parent_table: parent_table.clone(),
                        parent_columns: Vec::new(),
                        on_delete: on_delete.filter(|s| !s.is_empty() && s != "NO ACTION"),
                    });
                    entry.child_columns.push(child_col);
                    entry.parent_columns.push(parent_col);
                }
                out.extend(by_id.into_values());
            }
            Ok(out)
        })
        .await
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
    }

    async fn bulk_insert_rows(&mut self, _target: BulkInsert<'_>) -> Result<usize, SqlError> {
        // SQLite has no protocol-level bulk loader; its bottleneck
        // is fsync, not parse/plan. The generic multi-row INSERT in
        // copy.rs is already optimal for it. Always degrade to the
        // generic path.
        Err(SqlError::BulkUnavailable(
            "SQLite has no native bulk loader; multi-row INSERT is already optimal".into(),
        ))
    }
}

pub(crate) async fn connect(
    _url: &DatabaseUrl,
    _opts: &ConnectOptions,
) -> Result<SqliteConnection, SqlError> {
    let path = _url.path().to_string();
    tokio::task::spawn_blocking(move || {
        let conn =
            SqliteConn::open(&path).map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;
        Ok(SqliteConnection {
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        })
    })
    .await
    .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?
}

fn sqlite_to_value(v: SqliteValue) -> Value {
    match v {
        SqliteValue::Null => Value::Null,
        SqliteValue::Integer(i) => Value::Int64(i),
        SqliteValue::Real(f) => Value::Float64(f),
        SqliteValue::Text(s) => Value::String(s),
        SqliteValue::Blob(b) => Value::Bytes(b),
    }
}

fn escape_sqlite_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Split a SQL string into individual statements for SQLite.
///
/// Handles single-quoted strings (`''` escape), double-quoted identifiers
/// (`""` escape), `--` line comments, and `/* */` block comments.
fn split_sqlite_statements(sql: &str) -> Result<Vec<&str>, String> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let bytes = sql.as_bytes();

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b';' => {
                statements.push(&sql[start..=i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }

    if start < sql.len() {
        let tail = &sql[start..];
        if !tail.trim().is_empty() {
            statements.push(tail.trim_end());
        }
    }

    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Returns a fresh on-disk SQLite URL. Each call yields a unique path so
    /// concurrent tests do not collide.
    fn fresh_test_url() -> (String, std::path::PathBuf) {
        let pid = std::process::id();
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("ferrule-sqlite-test-{pid}-{n}.db"));
        let _ = std::fs::remove_file(&path);
        let url = format!("sqlite://{}", path.display());
        (url, path)
    }

    /// Connect to a fresh on-disk SQLite database, returning the connection
    /// and the path so the caller can clean up.
    fn fresh_conn() -> (Box<dyn crate::Connection>, std::path::PathBuf) {
        let (raw_url, path) = fresh_test_url();
        let url = DatabaseUrl::parse(&raw_url).expect("parse sqlite URL");
        let conn =
            crate::connect(&url, &ConnectOptions::default(), None).expect("connect should succeed");
        (conn, path)
    }

    /// Seed the standard test_users table; mirrors the schemas used for the
    /// other backends (see CLAUDE.md "How to Test").
    fn seed_test_users(conn: &mut dyn crate::Connection) {
        conn.execute(
            "CREATE TABLE test_users (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT,
                age INTEGER,
                score REAL,
                active INTEGER,
                meta TEXT
            )",
        )
        .expect("create table");
        conn.execute("INSERT INTO test_users (name, age, score, active, meta) VALUES ('Alice', 30, 99.5, 1, '{\"role\":\"admin\"}')")
            .expect("insert alice");
        conn.execute("INSERT INTO test_users (name, age, score, active, meta) VALUES ('Bob', 25, 88.25, 0, '{\"role\":\"user\"}')")
            .expect("insert bob");
    }

    #[test]
    fn test_sqlite_ping() {
        let (mut conn, path) = fresh_conn();
        conn.ping().expect("ping should succeed");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_query() {
        let (mut conn, path) = fresh_conn();
        seed_test_users(&mut conn);
        let result = conn
            .query("SELECT * FROM test_users ORDER BY id")
            .expect("query should succeed");
        assert_eq!(result.columns.len(), 6, "expected 6 columns");
        assert_eq!(result.rows.len(), 2, "expected 2 seeded rows");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_execute() {
        let (mut conn, path) = fresh_conn();
        seed_test_users(&mut conn);
        let summary = conn
            .execute("INSERT INTO test_users (name, age) VALUES ('Charlie', 35)")
            .expect("execute should succeed");
        assert_eq!(
            summary.rows_affected,
            Some(1),
            "expected exactly one row inserted"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_list_tables() {
        let (mut conn, path) = fresh_conn();
        seed_test_users(&mut conn);
        conn.execute("CREATE TABLE other (id INTEGER)")
            .expect("create other");
        let tables = conn.list_tables(None).expect("list_tables");
        assert!(tables.contains(&"test_users".to_string()));
        assert!(tables.contains(&"other".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_describe_table() {
        let (mut conn, path) = fresh_conn();
        seed_test_users(&mut conn);
        let result = conn.describe_table(None, "test_users").expect("describe");
        // PRAGMA table_info returns one row per column: cid, name, type, notnull, dflt_value, pk.
        assert!(
            result.rows.len() >= 6,
            "expected >=6 columns in test_users, got {}",
            result.rows.len()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_type_mapping() {
        let (mut conn, path) = fresh_conn();
        // Build a row that exercises each SqliteValue branch in sqlite_to_value.
        conn.execute(
            "CREATE TABLE typed (
                i INTEGER,
                r REAL,
                t TEXT,
                b BLOB,
                n INTEGER
            )",
        )
        .expect("create typed");
        conn.execute("INSERT INTO typed VALUES (42, 2.5, 'hi', x'deadbeef', NULL)")
            .expect("insert typed");

        let result = conn
            .query("SELECT i, r, t, b, n FROM typed")
            .expect("query typed");
        let row = &result.rows[0];
        assert!(matches!(row[0], Value::Int64(42)), "i should be Int64(42)");
        assert!(
            matches!(row[1], Value::Float64(f) if (f - 2.5).abs() < 1e-9),
            "r should be Float64(~2.5)"
        );
        assert!(
            matches!(&row[2], Value::String(s) if s == "hi"),
            "t should be String('hi')"
        );
        assert!(
            matches!(&row[3], Value::Bytes(b) if b == &vec![0xde, 0xad, 0xbe, 0xef]),
            "b should be Bytes(0xDEADBEEF)"
        );
        assert!(matches!(row[4], Value::Null), "n should be Null");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_execute_multi() {
        let (mut conn, path) = fresh_conn();
        let results = conn
            .execute_multi(
                "CREATE TABLE m (id INTEGER); \
                 INSERT INTO m VALUES (1); \
                 INSERT INTO m VALUES (2); \
                 SELECT COUNT(*) AS c FROM m;",
            )
            .expect("execute_multi");
        assert_eq!(results.len(), 4, "expected 4 statement results");
        match results.last().unwrap() {
            StatementResult::Query(qr) => {
                assert_eq!(qr.rows.len(), 1);
                assert!(matches!(qr.rows[0][0], Value::Int64(2)));
            }
            other => panic!("last result should be Query, got {:?}", other),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_escape_sqlite_identifier_doubles_quotes() {
        assert_eq!(escape_sqlite_identifier("plain"), "\"plain\"");
        assert_eq!(escape_sqlite_identifier("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn test_sqlite_primary_key() {
        let (mut conn, path) = fresh_conn();
        seed_test_users(&mut conn);
        let pk = conn.primary_key(None, "test_users").expect("primary_key");
        assert_eq!(pk, vec!["id".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_primary_key_composite_in_order() {
        let (mut conn, path) = fresh_conn();
        // SQLite uses the column order in the PRIMARY KEY clause for
        // the `pk` ordinal: tenant first, then resource.
        conn.execute(
            "CREATE TABLE membership (
                tenant TEXT,
                resource TEXT,
                role TEXT,
                PRIMARY KEY (tenant, resource)
            )",
        )
        .expect("create membership");
        let pk = conn.primary_key(None, "membership").expect("primary_key");
        assert_eq!(pk, vec!["tenant".to_string(), "resource".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_primary_key_none() {
        let (mut conn, path) = fresh_conn();
        conn.execute("CREATE TABLE no_pk (a INTEGER, b TEXT)")
            .expect("create no_pk");
        let pk = conn.primary_key(None, "no_pk").expect("primary_key");
        assert!(pk.is_empty(), "expected no PK columns, got {pk:?}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_list_foreign_keys() {
        let (mut conn, path) = fresh_conn();
        seed_test_users(&mut conn);
        conn.execute(
            "CREATE TABLE test_orders (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id INTEGER REFERENCES test_users(id) ON DELETE CASCADE,
                total REAL
            )",
        )
        .expect("create test_orders");
        let fks = conn.list_foreign_keys(None).expect("list_foreign_keys");
        assert_eq!(fks.len(), 1, "expected one FK edge, got {fks:?}");
        let fk = &fks[0];
        assert_eq!(fk.child_table, "test_orders");
        assert_eq!(fk.child_columns, vec!["user_id".to_string()]);
        assert_eq!(fk.parent_table, "test_users");
        assert_eq!(fk.parent_columns, vec!["id".to_string()]);
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_sqlite_list_foreign_keys_composite() {
        let (mut conn, path) = fresh_conn();
        conn.execute(
            "CREATE TABLE parent (
                a INTEGER, b INTEGER,
                PRIMARY KEY (a, b)
            )",
        )
        .expect("create parent");
        conn.execute(
            "CREATE TABLE child (
                x INTEGER, y INTEGER,
                FOREIGN KEY (x, y) REFERENCES parent(a, b)
            )",
        )
        .expect("create child");
        let fks = conn.list_foreign_keys(None).expect("list_foreign_keys");
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].child_columns, vec!["x".to_string(), "y".to_string()]);
        assert_eq!(
            fks[0].parent_columns,
            vec!["a".to_string(), "b".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }
}
