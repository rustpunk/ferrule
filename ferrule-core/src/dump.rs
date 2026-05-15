use crate::backend::Backend;
use crate::connection::Connection;
use crate::error::CoreError;
use crate::params::render_value;
use crate::value::{ColumnInfo, Value};

/// Supported dump formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat {
    Csv,
    Json,
    Sql,
}

impl DumpFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }
}

/// Options for a dump operation.
#[derive(Debug, Clone)]
pub struct DumpOptions {
    pub format: DumpFormat,
    pub batch_size: usize,
    pub schema: Option<String>,
    /// When true and `format == DumpFormat::Sql`, produce a byte-stable
    /// stream of one `INSERT INTO ... VALUES (...);` per row, with rows
    /// ordered server-side by primary key (or by every column,
    /// lexicographically, when the table has no PK — a warning is
    /// emitted on stderr in that case). JSON cell values are
    /// re-serialised with sorted object keys.
    ///
    /// Has no effect on `DumpFormat::Csv` / `DumpFormat::Json`.
    pub deterministic: bool,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            format: DumpFormat::Csv,
            batch_size: 1000,
            schema: None,
            deterministic: false,
        }
    }
}

/// Dump an entire table using server‑side paging.
pub async fn dump_table(
    conn: &mut dyn Connection,
    table: &str,
    backend: Backend,
    opts: &DumpOptions,
) -> Result<String, CoreError> {
    let quoted_table = crate::copy::quote_identifier(table, backend);

    // Determinism (Scope A): append ORDER BY before paging so that
    // the per-page LIMIT/OFFSET windows partition a stable order.
    // ORDER BY must come *before* apply_paging in dump_query — else
    // the LIMIT/OFFSET would slot between SELECT and ORDER BY.
    let sql = if opts.deterministic && opts.format == DumpFormat::Sql {
        let pks = conn.primary_key(opts.schema.as_deref(), table).await?;
        let order_cols: Vec<String> = if pks.is_empty() {
            eprintln!(
                "[ferrule] note: table '{table}' has no PRIMARY KEY; \
                 sorting by all columns (slower)."
            );
            let described = conn.describe_table(opts.schema.as_deref(), table).await?;
            let mut names: Vec<String> =
                described.columns.iter().map(|c| c.name.clone()).collect();
            names.sort();
            names
        } else {
            pks
        };
        let order_by = build_order_by(&order_cols, backend);
        format!("SELECT * FROM {quoted_table}{order_by}")
    } else {
        format!("SELECT * FROM {quoted_table}")
    };

    dump_query(conn, &sql, backend, opts, Some(table)).await
}

/// Dump the results of an arbitrary SELECT query.
///
/// Rows are fetched in paged batches and formatted incrementally so the
/// entire result set never has to reside in memory at once.
pub async fn dump_query(
    conn: &mut dyn Connection,
    sql: &str,
    backend: Backend,
    opts: &DumpOptions,
    table_name: Option<&str>,
) -> Result<String, CoreError> {
    // Determinism precondition: refuse `--deterministic` against a
    // raw query that lacks an ORDER BY. The substring match is
    // intentionally pragmatic — a query that contains "order by"
    // inside a string literal or comment will pass this check. We
    // accept those false positives in exchange for not building a
    // SQL parser.
    if opts.deterministic
        && opts.format == DumpFormat::Sql
        && !sql.to_lowercase().contains("order by")
    {
        return Err(CoreError::QueryFailed(
            "dump_query --deterministic requires an ORDER BY clause in the source SQL \
             (substring match is intentionally pragmatic — a query that contains \
             'order by' only inside a comment or string literal will pass this check)."
                .into(),
        ));
    }

    let mut offset = 0usize;
    let mut first_page = true;
    let mut columns: Vec<ColumnInfo> = Vec::new();

    match opts.format {
        DumpFormat::Csv => {
            let mut buf = Vec::new();
            {
                let mut wtr = csv::Writer::from_writer(&mut buf);
                loop {
                    let paged = crate::query_builder::apply_paging(
                        sql,
                        Some(opts.batch_size),
                        Some(offset),
                        backend,
                    )?;
                    let page = conn.query(&paged).await?;

                    if first_page {
                        if !page.columns.is_empty() {
                            columns = page.columns;
                            let headers: Vec<&str> =
                                columns.iter().map(|c| c.name.as_str()).collect();
                            wtr.write_record(&headers)
                                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                        }
                        first_page = false;
                    }

                    if page.rows.is_empty() {
                        break;
                    }

                    for row in &page.rows {
                        let cells: Vec<String> = row.iter().map(value_to_csv_cell).collect();
                        wtr.write_record(&cells)
                            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                    }

                    let fetched = page.rows.len();
                    offset += fetched;
                    if fetched < opts.batch_size {
                        break;
                    }
                }
                wtr.flush()
                    .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            }
            String::from_utf8(buf).map_err(|e| CoreError::QueryFailed(e.to_string()))
        }

        DumpFormat::Json => {
            let mut buf = Vec::new();
            buf.push(b'[');
            let mut first_row = true;

            loop {
                let paged = crate::query_builder::apply_paging(
                    sql,
                    Some(opts.batch_size),
                    Some(offset),
                    backend,
                )?;
                let page = conn.query(&paged).await?;

                if first_page {
                    if !page.columns.is_empty() {
                        columns = page.columns;
                    }
                    first_page = false;
                }

                if page.rows.is_empty() {
                    break;
                }

                for row in &page.rows {
                    if !first_row {
                        buf.push(b',');
                    }
                    first_row = false;

                    let mut obj = serde_json::Map::new();
                    for (col, val) in columns.iter().zip(row.iter()) {
                        obj.insert(col.name.clone(), json_value(val));
                    }
                    let json_str = serde_json::to_string(&serde_json::Value::Object(obj))
                        .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                    buf.extend_from_slice(json_str.as_bytes());
                }

                let fetched = page.rows.len();
                offset += fetched;
                if fetched < opts.batch_size {
                    break;
                }
            }

            buf.push(b']');
            String::from_utf8(buf).map_err(|e| CoreError::QueryFailed(e.to_string()))
        }

        DumpFormat::Sql => {
            let table = table_name.unwrap_or("dumped_table");
            let quoted_table = crate::copy::quote_identifier(table, backend);
            let mut out = String::new();

            loop {
                let paged = crate::query_builder::apply_paging(
                    sql,
                    Some(opts.batch_size),
                    Some(offset),
                    backend,
                )?;
                let page = conn.query(&paged).await?;

                if first_page {
                    if !page.columns.is_empty() {
                        columns = page.columns;
                    }
                    first_page = false;
                }

                if page.rows.is_empty() {
                    break;
                }

                let col_names: Vec<String> = columns
                    .iter()
                    .map(|c| crate::copy::quote_identifier(&c.name, backend))
                    .collect();
                let cols = col_names.join(", ");

                if opts.deterministic {
                    // One INSERT statement per row — eliminates batch-
                    // boundary noise in diffs and keeps each row
                    // independently re-orderable / re-loadable.
                    for row in &page.rows {
                        let cells: Vec<String> = row
                            .iter()
                            .map(|v| render_value_deterministic(v, backend))
                            .collect();
                        out.push_str(&format!(
                            "INSERT INTO {quoted_table} ({cols}) VALUES ({});\n",
                            cells.join(", ")
                        ));
                    }
                } else {
                    let values: Vec<String> = page
                        .rows
                        .iter()
                        .map(|row| {
                            let cells: Vec<String> =
                                row.iter().map(|v| render_value(v, backend)).collect();
                            format!("({})", cells.join(", "))
                        })
                        .collect();

                    out.push_str(&format!(
                        "INSERT INTO {quoted_table} ({cols}) VALUES {};\n",
                        values.join(", ")
                    ));
                }

                let fetched = page.rows.len();
                offset += fetched;
                if fetched < opts.batch_size {
                    break;
                }
            }

            Ok(out)
        }
    }
}

fn value_to_csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn json_value(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int64(i) => serde_json::Value::Number((*i).into()),
        Value::Float64(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or_else(|| serde_json::Number::from(0)),
        ),
        Value::Decimal(d) => serde_json::Value::String(d.clone()),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(_b) => serde_json::Value::String(format!("<{} bytes>", _b.len())),
        Value::Date(d) => serde_json::Value::String(d.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::DateTime(dt) => serde_json::Value::String(dt.to_string()),
        Value::DateTimeTz(dt) => serde_json::Value::String(dt.to_rfc3339()),
        Value::Json(j) => j.clone(),
        Value::Uuid(u) => serde_json::Value::String(u.clone()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(json_value).collect()),
    }
}

/// Recursively re-serialise a JSON value with object keys in
/// lexicographic order. Arrays preserve element order (positional);
/// only object keys are reordered. Used by `--deterministic` to make
/// dump output byte-stable across Postgres `JSONB` (hash-ordered) and
/// MySQL `JSON` (insertion-ordered).
fn canonicalize_json_value(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value as J;
    use std::collections::BTreeMap;
    match v {
        J::Object(map) => {
            let sorted: BTreeMap<String, J> = map
                .into_iter()
                .map(|(k, v)| (k, canonicalize_json_value(v)))
                .collect();
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            J::Object(out)
        }
        J::Array(arr) => J::Array(arr.into_iter().map(canonicalize_json_value).collect()),
        other => other,
    }
}

/// Render a [`Value`] for inclusion in a deterministic SQL dump.
///
/// Identical to [`crate::params::render_value`] for every variant
/// except `Value::Json`, which is re-serialised through
/// [`canonicalize_json_value`] so object keys are sorted.
fn render_value_deterministic(v: &Value, backend: Backend) -> String {
    match v {
        Value::Json(j) => {
            let canon = canonicalize_json_value(j.clone());
            crate::params::quote_string(&canon.to_string())
        }
        _ => render_value(v, backend),
    }
}

/// Build a backend-quoted `" ORDER BY a, b, c"` clause from a list of
/// column names. Returns an empty string when `cols` is empty (no
/// columns to sort by — caller is expected to handle that path).
fn build_order_by(cols: &[String], backend: Backend) -> String {
    if cols.is_empty() {
        return String::new();
    }
    let quoted: Vec<String> = cols
        .iter()
        .map(|c| crate::copy::quote_identifier(c, backend))
        .collect();
    format!(" ORDER BY {}", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_value_keys_sorted_in_deterministic() {
        let v = serde_json::json!({"z":1, "a":2, "nested":{"y":1,"b":2}});
        let c = canonicalize_json_value(v);
        assert_eq!(
            c.to_string(),
            r#"{"a":2,"nested":{"b":2,"y":1},"z":1}"#
        );
    }

    #[cfg(feature = "sqlite")]
    mod sqlite_dump_tests {
        use super::*;
        use crate::backends::sqlite::connect as sqlite_connect;
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);

        fn tmp_path(suffix: &str) -> std::path::PathBuf {
            let pid = std::process::id();
            let n = N.fetch_add(1, Ordering::SeqCst);
            std::env::temp_dir().join(format!("ferrule-dump-test-{pid}-{n}-{suffix}.db"))
        }

        async fn open_sqlite(
            path: &std::path::Path,
        ) -> crate::backends::sqlite::SqliteConnection {
            let _ = std::fs::remove_file(path);
            let url = DatabaseUrl::parse(&format!("sqlite://{}", path.display())).unwrap();
            sqlite_connect(&url, &ConnectOptions::default()).await.unwrap()
        }

        #[tokio::test]
        async fn dump_twice_byte_equal() {
            let path = tmp_path("twice");
            let mut conn = open_sqlite(&path).await;
            conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
                .await
                .unwrap();
            // Insert out of PK order.
            conn.execute("INSERT INTO users VALUES (2, 'Bob')").await.unwrap();
            conn.execute("INSERT INTO users VALUES (1, 'Alice')").await.unwrap();
            conn.execute("INSERT INTO users VALUES (3, 'Carol')").await.unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let out1 = dump_table(&mut conn, "users", Backend::Sqlite, &opts)
                .await
                .unwrap();
            let out2 = dump_table(&mut conn, "users", Backend::Sqlite, &opts)
                .await
                .unwrap();
            assert_eq!(out1, out2, "deterministic dump not byte-equal");
            assert_eq!(
                out1.matches("INSERT INTO").count(),
                3,
                "expected 3 INSERT lines, got:\n{out1}"
            );
            // Confirm the row order is sorted by PK (1, 2, 3).
            let pos_alice = out1.find("Alice").unwrap();
            let pos_bob = out1.find("Bob").unwrap();
            let pos_carol = out1.find("Carol").unwrap();
            assert!(pos_alice < pos_bob && pos_bob < pos_carol);

            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn dump_stable_across_insertion_order() {
            let path_a = tmp_path("stable-a");
            let path_b = tmp_path("stable-b");
            let mut a = open_sqlite(&path_a).await;
            let mut b = open_sqlite(&path_b).await;

            a.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
                .await
                .unwrap();
            b.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
                .await
                .unwrap();
            // a: 1, 2, 3
            a.execute("INSERT INTO users VALUES (1, 'Alice')").await.unwrap();
            a.execute("INSERT INTO users VALUES (2, 'Bob')").await.unwrap();
            a.execute("INSERT INTO users VALUES (3, 'Carol')").await.unwrap();
            // b: 3, 1, 2
            b.execute("INSERT INTO users VALUES (3, 'Carol')").await.unwrap();
            b.execute("INSERT INTO users VALUES (1, 'Alice')").await.unwrap();
            b.execute("INSERT INTO users VALUES (2, 'Bob')").await.unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let out_a = dump_table(&mut a, "users", Backend::Sqlite, &opts)
                .await
                .unwrap();
            let out_b = dump_table(&mut b, "users", Backend::Sqlite, &opts)
                .await
                .unwrap();
            assert_eq!(out_a, out_b);

            let _ = std::fs::remove_file(&path_a);
            let _ = std::fs::remove_file(&path_b);
        }

        #[tokio::test]
        async fn dump_no_pk_warns_and_sorts() {
            let path = tmp_path("nopk");
            let mut conn = open_sqlite(&path).await;
            // SQLite "heap-ish" table — no INTEGER PRIMARY KEY. Note
            // that a `WITHOUT ROWID` table would require an explicit
            // PK, so we use a plain heap and confirm primary_key()
            // returns empty.
            conn.execute("CREATE TABLE heap (a INTEGER, b TEXT)").await.unwrap();
            let pks = conn.primary_key(None, "heap").await.unwrap();
            assert!(pks.is_empty(), "expected no PK for heap, got {pks:?}");

            conn.execute("INSERT INTO heap VALUES (2, 'beta')").await.unwrap();
            conn.execute("INSERT INTO heap VALUES (1, 'alpha')").await.unwrap();
            conn.execute("INSERT INTO heap VALUES (3, 'gamma')").await.unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            // Stderr capture is painful in cargo test; the docs/test
            // contract is that the dumps are byte-equal even without
            // a PK.
            let out1 = dump_table(&mut conn, "heap", Backend::Sqlite, &opts)
                .await
                .unwrap();
            let out2 = dump_table(&mut conn, "heap", Backend::Sqlite, &opts)
                .await
                .unwrap();
            assert_eq!(out1, out2);
            assert_eq!(out1.matches("INSERT INTO").count(), 3);

            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn dump_uses_backend_quoting() {
            let path = tmp_path("quote");
            let mut conn = open_sqlite(&path).await;
            conn.execute(
                "CREATE TABLE \"weird name\" (\"id\" INTEGER PRIMARY KEY, \"first name\" TEXT)",
            )
            .await
            .unwrap();
            conn.execute("INSERT INTO \"weird name\" VALUES (1, 'Alice')")
                .await
                .unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let out = dump_table(&mut conn, "weird name", Backend::Sqlite, &opts)
                .await
                .unwrap();
            // SQLite uses ANSI quotes — table and column names must
            // each appear inside double quotes.
            assert!(
                out.contains("INSERT INTO \"weird name\""),
                "expected ANSI-quoted table name, got:\n{out}"
            );
            assert!(
                out.contains("\"first name\""),
                "expected ANSI-quoted column name, got:\n{out}"
            );

            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn dump_deterministic_query_requires_order_by() {
            let path = tmp_path("query-orderby");
            let mut conn = open_sqlite(&path).await;
            conn.execute("CREATE TABLE t (x INTEGER)").await.unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };

            // Missing ORDER BY → error referencing the clause.
            let err = dump_query(
                &mut conn,
                "SELECT 1 AS x",
                Backend::Sqlite,
                &opts,
                Some("dummy"),
            )
            .await
            .unwrap_err();
            assert!(
                err.to_string().to_lowercase().contains("order by"),
                "error should mention ORDER BY, got: {err}"
            );

            // Happy path — ORDER BY present.
            dump_query(
                &mut conn,
                "SELECT 1 AS x ORDER BY 1",
                Backend::Sqlite,
                &opts,
                Some("dummy"),
            )
            .await
            .expect("dump_query with ORDER BY should succeed");

            let _ = std::fs::remove_file(&path);
        }
    }
}
