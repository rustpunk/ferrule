use ferrule_sql::render_value;
use ferrule_sql::value::{ColumnInfo, Value};
use ferrule_sql::{Backend, Connection, SqlError};
use std::fmt::Write as _;

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

/// Dump an entire table by streaming a single native cursor.
pub fn dump_table(
    conn: &mut dyn Connection,
    table: &str,
    backend: Backend,
    opts: &DumpOptions,
) -> Result<String, SqlError> {
    let quoted_table = ferrule_sql::copy::quote_identifier(table, backend);

    // Determinism (Scope A): append ORDER BY to the SELECT so the
    // single streaming cursor yields rows in a stable, server-side
    // order. (No LIMIT/OFFSET windowing is involved any more — one
    // ordered SELECT streamed through one cursor is strictly more
    // stable than the old paged path.)
    let sql = if opts.deterministic && opts.format == DumpFormat::Sql {
        let pks = conn.primary_key(opts.schema.as_deref(), table)?;
        let order_cols: Vec<String> = if pks.is_empty() {
            eprintln!(
                "[ferrule] note: table '{table}' has no PRIMARY KEY; \
                 sorting by all columns (slower)."
            );
            let described = conn.describe_table(opts.schema.as_deref(), table)?;
            let mut names: Vec<String> = described.columns.iter().map(|c| c.name.clone()).collect();
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

    dump_query(conn, &sql, backend, opts, Some(table))
}

/// Dump the results of an arbitrary SELECT query.
///
/// Rows are streamed from a single native database cursor in bounded
/// `batch_size` chunks, so fetch memory stays `O(batch)` rather than
/// re-issuing an `O(offset)` LIMIT/OFFSET query per page. The assembled
/// output is still buffered into the returned `String` (true
/// row-to-writer streaming is a separate, deferred signature change).
pub fn dump_query(
    conn: &mut dyn Connection,
    sql: &str,
    backend: Backend,
    opts: &DumpOptions,
    table_name: Option<&str>,
) -> Result<String, SqlError> {
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
        return Err(SqlError::QueryFailed(
            "dump_query --deterministic requires an ORDER BY clause in the source SQL \
             (substring match is intentionally pragmatic — a query that contains \
             'order by' only inside a comment or string literal will pass this check)."
                .into(),
        ));
    }

    // Stream rows from a single native cursor (#55-adjacent debt: was an
    // O(n^2) LIMIT/OFFSET re-query loop). The cursor borrows `conn` for
    // its whole lifetime, so any introspection call (`primary_key`,
    // `describe_table`) must already have run — `dump_table` does that
    // before it reaches here. Fetch memory is bounded to one
    // `batch_size` chunk; the assembled output string is still buffered
    // by value (true row-to-writer streaming is a separate signature
    // change, deferred).
    let mut cursor = conn.query_cursor(sql)?;
    let columns: Vec<ColumnInfo> = cursor.columns().to_vec();

    match opts.format {
        DumpFormat::Csv => {
            let mut buf = Vec::new();
            {
                let mut wtr = csv::Writer::from_writer(&mut buf);
                if !columns.is_empty() {
                    let headers: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
                    wtr.write_record(&headers)
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                }
                loop {
                    let batch = cursor.next_batch(opts.batch_size)?;
                    if batch.is_empty() {
                        break;
                    }
                    for row in &batch {
                        let cells: Vec<String> = row.iter().map(value_to_csv_cell).collect();
                        wtr.write_record(&cells)
                            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    }
                }
                wtr.flush()
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
            }
            String::from_utf8(buf).map_err(|e| SqlError::QueryFailed(e.to_string()))
        }

        DumpFormat::Json => {
            let mut buf = Vec::new();
            buf.push(b'[');
            let mut first_row = true;

            loop {
                let batch = cursor.next_batch(opts.batch_size)?;
                if batch.is_empty() {
                    break;
                }
                for row in &batch {
                    if !first_row {
                        buf.push(b',');
                    }
                    first_row = false;

                    let mut obj = serde_json::Map::new();
                    for (col, val) in columns.iter().zip(row.iter()) {
                        obj.insert(col.name.clone(), json_value(val));
                    }
                    let json_str = serde_json::to_string(&serde_json::Value::Object(obj))
                        .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                    buf.extend_from_slice(json_str.as_bytes());
                }
            }

            buf.push(b']');
            String::from_utf8(buf).map_err(|e| SqlError::QueryFailed(e.to_string()))
        }

        DumpFormat::Sql => {
            let table = table_name.unwrap_or("dumped_table");
            let quoted_table = ferrule_sql::copy::quote_identifier(table, backend);
            let col_names: Vec<String> = columns
                .iter()
                .map(|c| ferrule_sql::copy::quote_identifier(&c.name, backend))
                .collect();
            let cols = col_names.join(", ");
            let mut out = String::new();

            loop {
                let batch = cursor.next_batch(opts.batch_size)?;
                if batch.is_empty() {
                    break;
                }

                if opts.deterministic {
                    // One INSERT statement per row — eliminates batch-
                    // boundary noise in diffs and keeps each row
                    // independently re-orderable / re-loadable. Stream
                    // straight into `out` to avoid a per-row Vec<String>
                    // and an intermediate format!() temp.
                    for row in &batch {
                        let _ = write!(&mut out, "INSERT INTO {quoted_table} ({cols}) VALUES (");
                        for (i, v) in row.iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            out.push_str(&render_value_deterministic(v, backend));
                        }
                        out.push_str(");\n");
                    }
                } else {
                    // One batched VALUES list per cursor chunk — keeps the
                    // non-deterministic dump compact. (A chunk maps to one
                    // `INSERT` statement, as the batched path always has.)
                    let values: Vec<String> = batch
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
/// Identical to [`ferrule_sql::render_value`] for every variant
/// except `Value::Json`, which is re-serialised through
/// [`canonicalize_json_value`] so object keys are sorted.
fn render_value_deterministic(v: &Value, backend: Backend) -> String {
    match v {
        Value::Json(j) => {
            let canon = canonicalize_json_value(j.clone());
            ferrule_sql::quote_string(&canon.to_string())
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
        .map(|c| ferrule_sql::copy::quote_identifier(c, backend))
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
        assert_eq!(c.to_string(), r#"{"a":2,"nested":{"b":2,"y":1},"z":1}"#);
    }

    #[cfg(feature = "sqlite")]
    mod sqlite_dump_tests {
        use super::*;
        use ferrule_sql::ConnectOptions;
        use ferrule_sql::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);

        fn tmp_path(suffix: &str) -> std::path::PathBuf {
            let pid = std::process::id();
            let n = N.fetch_add(1, Ordering::SeqCst);
            std::env::temp_dir().join(format!("ferrule-dump-test-{pid}-{n}-{suffix}.db"))
        }

        fn open_sqlite(path: &std::path::Path) -> Box<dyn ferrule_sql::Connection> {
            let _ = std::fs::remove_file(path);
            let url = DatabaseUrl::parse(&format!("sqlite://{}", path.display())).unwrap();
            ferrule_sql::connect(&url, &ConnectOptions::default(), None).unwrap()
        }

        #[test]
        fn dump_twice_byte_equal() {
            let path = tmp_path("twice");
            let mut conn = open_sqlite(&path);
            conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
                .unwrap();
            // Insert out of PK order.
            conn.execute("INSERT INTO users VALUES (2, 'Bob')").unwrap();
            conn.execute("INSERT INTO users VALUES (1, 'Alice')")
                .unwrap();
            conn.execute("INSERT INTO users VALUES (3, 'Carol')")
                .unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let out1 = dump_table(&mut conn, "users", Backend::Sqlite, &opts).unwrap();
            let out2 = dump_table(&mut conn, "users", Backend::Sqlite, &opts).unwrap();
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

        #[test]
        fn dump_stable_across_insertion_order() {
            let path_a = tmp_path("stable-a");
            let path_b = tmp_path("stable-b");
            let mut a = open_sqlite(&path_a);
            let mut b = open_sqlite(&path_b);

            a.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
                .unwrap();
            b.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
                .unwrap();
            // a: 1, 2, 3
            a.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
            a.execute("INSERT INTO users VALUES (2, 'Bob')").unwrap();
            a.execute("INSERT INTO users VALUES (3, 'Carol')").unwrap();
            // b: 3, 1, 2
            b.execute("INSERT INTO users VALUES (3, 'Carol')").unwrap();
            b.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
            b.execute("INSERT INTO users VALUES (2, 'Bob')").unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let out_a = dump_table(&mut a, "users", Backend::Sqlite, &opts).unwrap();
            let out_b = dump_table(&mut b, "users", Backend::Sqlite, &opts).unwrap();
            assert_eq!(out_a, out_b);

            let _ = std::fs::remove_file(&path_a);
            let _ = std::fs::remove_file(&path_b);
        }

        #[test]
        fn dump_no_pk_warns_and_sorts() {
            let path = tmp_path("nopk");
            let mut conn = open_sqlite(&path);
            // SQLite "heap-ish" table — no INTEGER PRIMARY KEY. Note
            // that a `WITHOUT ROWID` table would require an explicit
            // PK, so we use a plain heap and confirm primary_key()
            // returns empty.
            conn.execute("CREATE TABLE heap (a INTEGER, b TEXT)")
                .unwrap();
            let pks = conn.primary_key(None, "heap").unwrap();
            assert!(pks.is_empty(), "expected no PK for heap, got {pks:?}");

            conn.execute("INSERT INTO heap VALUES (2, 'beta')").unwrap();
            conn.execute("INSERT INTO heap VALUES (1, 'alpha')")
                .unwrap();
            conn.execute("INSERT INTO heap VALUES (3, 'gamma')")
                .unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            // Stderr capture is painful in cargo test; the docs/test
            // contract is that the dumps are byte-equal even without
            // a PK.
            let out1 = dump_table(&mut conn, "heap", Backend::Sqlite, &opts).unwrap();
            let out2 = dump_table(&mut conn, "heap", Backend::Sqlite, &opts).unwrap();
            assert_eq!(out1, out2);
            assert_eq!(out1.matches("INSERT INTO").count(), 3);

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn dump_uses_backend_quoting() {
            let path = tmp_path("quote");
            let mut conn = open_sqlite(&path);
            conn.execute(
                "CREATE TABLE \"weird name\" (\"id\" INTEGER PRIMARY KEY, \"first name\" TEXT)",
            )
            .unwrap();
            conn.execute("INSERT INTO \"weird name\" VALUES (1, 'Alice')")
                .unwrap();

            let opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let out = dump_table(&mut conn, "weird name", Backend::Sqlite, &opts).unwrap();
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

        // #55-adjacent: the cursor rewrite must span multiple batches.
        // 2500 rows > the 1000-row batch_size AND >
        // DEFAULT_CURSOR_CAPACITY (1024), so every format walks the
        // `next_batch` loop several times.
        #[test]
        fn dump_streams_across_multiple_batches() {
            let path = tmp_path("multibatch");
            let mut conn = open_sqlite(&path);
            conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
                .unwrap();
            const N: usize = 2500;
            // Bulk insert in one statement to keep the fixture fast.
            let mut sql = String::from("INSERT INTO t (id, v) VALUES ");
            for i in 0..N {
                if i > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(&format!("({}, 'row-{}')", i + 1, i + 1));
            }
            conn.execute(&sql).unwrap();

            // CSV: N data rows + 1 header line.
            let csv_opts = DumpOptions {
                format: DumpFormat::Csv,
                ..Default::default()
            };
            let csv = dump_table(&mut conn, "t", Backend::Sqlite, &csv_opts).unwrap();
            let csv_lines = csv.lines().count();
            assert_eq!(csv_lines, N + 1, "CSV should have N data + 1 header line");

            // JSON: parses to an array of exactly N objects.
            let json_opts = DumpOptions {
                format: DumpFormat::Json,
                ..Default::default()
            };
            let json = dump_table(&mut conn, "t", Backend::Sqlite, &json_opts).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed.as_array().map(|a| a.len()),
                Some(N),
                "JSON should hold N objects"
            );

            // SQL (deterministic = one INSERT per row): N INSERT lines.
            let sql_opts = DumpOptions {
                format: DumpFormat::Sql,
                deterministic: true,
                ..Default::default()
            };
            let dump = dump_table(&mut conn, "t", Backend::Sqlite, &sql_opts).unwrap();
            assert_eq!(
                dump.matches("INSERT INTO").count(),
                N,
                "deterministic SQL should have N INSERT lines"
            );
            // First and last rows are present and ordered by PK.
            let first = dump.find("row-1\'").unwrap();
            let last = dump.find(&format!("row-{N}\'")).unwrap();
            assert!(first < last, "rows should be PK-ordered across batches");

            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn dump_deterministic_query_requires_order_by() {
            let path = tmp_path("query-orderby");
            let mut conn = open_sqlite(&path);
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();

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
            .expect("dump_query with ORDER BY should succeed");

            let _ = std::fs::remove_file(&path);
        }
    }
}
