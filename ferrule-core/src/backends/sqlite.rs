#![allow(dead_code, unused_variables, unused_imports)]

use async_trait::async_trait;
use crate::connection::{Connection, ConnectOptions, ExecutionSummary, QueryResult, StatementResult};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use rusqlite::Connection as SqliteConn;
use rusqlite::types::Value as SqliteValue;

pub struct SqliteConnection {
    conn: std::sync::Arc<std::sync::Mutex<SqliteConn>>,
}

#[async_trait]
impl Connection for SqliteConnection {
    async fn execute(
        &mut self,
        sql: &str,
    ) -> Result<ExecutionSummary, CoreError> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let affected = guard
                .execute(&sql, [])
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            Ok(ExecutionSummary {
                rows_affected: Some(affected as u64),
                command_tag: None,
            })
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn query(
        &mut self,
        sql: &str,
    ) -> Result<QueryResult, CoreError> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let mut stmt = guard
                .prepare(&sql)
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            let col_names = stmt.column_names();
            if col_names.is_empty() {
                return Err(CoreError::QueryFailed("Statement does not return rows".to_string()));
            }
            let columns: Vec<ColumnInfo> = col_names.iter().map(|name| {
                ColumnInfo {
                    name: name.to_string(),
                    type_hint: TypeHint::Other,
                    nullable: true,
                }
            }).collect();

            let mut rows = Vec::new();
            let mut rows_iter = stmt
                .query([])
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            while let Some(row) = rows_iter
                .next()
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?
            {
                let mut values = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    let val: SqliteValue = row
                        .get(i)
                        .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                    values.push(sqlite_to_value(val));
                }
                rows.push(values);
            }

            Ok(QueryResult { columns, rows })
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    // execute_multi uses the default impl: tries query(), falls back to execute()

    async fn execute_multi(
        &mut self,
        sql: &str,
    ) -> Result<Vec<StatementResult>, CoreError> {
        let statements = split_sqlite_statements(sql)
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        let mut results = Vec::with_capacity(statements.len());
        for stmt in statements {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            match self.query(stmt).await {
                Ok(result) => results.push(StatementResult::Query(result)),
                Err(CoreError::QueryFailed(_)) => {
                    let summary = self.execute(stmt).await?;
                    results.push(StatementResult::Summary(summary));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        let _ = self.query("SELECT 1").await?;
        Ok(())
    }

    async fn list_tables(
        &mut self,
        _schema: Option<&str>,
    ) -> Result<Vec<String>, CoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let mut stmt = guard
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            let names: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            Ok(names)
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn describe_table(
        &mut self,
        _schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, CoreError> {
        let table = table.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().unwrap();
            let sql = format!("PRAGMA table_info({})", escape_sqlite_identifier(&table));
            let mut stmt = guard
                .prepare(&sql)
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            let col_names = stmt.column_names();
            let columns: Vec<ColumnInfo> = col_names.iter().map(|name| {
                ColumnInfo {
                    name: name.to_string(),
                    type_hint: TypeHint::String,
                    nullable: true,
                }
            }).collect();
            let mut rows = Vec::new();
            let mut rows_iter = stmt
                .query([])
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            while let Some(row) = rows_iter
                .next()
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?
            {
                let mut values = Vec::with_capacity(columns.len());
                for i in 0..columns.len() {
                    let val: SqliteValue = row
                        .get(i)
                        .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                    values.push(sqlite_to_value(val));
                }
                rows.push(values);
            }
            Ok(QueryResult { columns, rows })
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }
}

pub async fn connect(_url: &DatabaseUrl, _opts: &ConnectOptions) -> Result<SqliteConnection, CoreError> {
    let path = _url.path().to_string();
    tokio::task::spawn_blocking(move || {
        let conn = SqliteConn::open(&path)
            .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;
        Ok(SqliteConnection {
            conn: std::sync::Arc::new(std::sync::Mutex::new(conn)),
        })
    })
    .await
    .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?
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
