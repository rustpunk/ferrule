//! Cross-DB row copy: stream rows from one backend's table or query
//! into another backend's table, translating types via the unified
//! [`Value`](crate::value::Value) enum.
//!
//! The default conflict policy is non-destructive — a copy into a
//! non-empty existing target table errors out before any INSERT (or
//! source SELECT) runs. Callers opt in to `Append` or `Truncate` via
//! [`IfExists`].

use crate::backend::Backend;
use crate::connection::Connection;
use crate::error::CoreError;
use crate::params::render_value;
use crate::value::{ColumnInfo, TypeHint, Value};

/// What to do when the target table already exists and is non-empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IfExists {
    /// Refuse to copy. Pre-flights the target with `SELECT 1 ... LIMIT 1`
    /// before issuing any source SELECT.
    #[default]
    Error,
    /// Insert alongside existing rows. UNIQUE/PK conflicts surface as
    /// driver errors and abort the run with already-committed batches
    /// still present on the target.
    Append,
    /// `DELETE FROM <tbl>` then insert. Destructive. Wrapped together
    /// with the first batch in a backend-aware transaction so a transient
    /// failure of the first INSERT cannot leave the target wiped + empty.
    Truncate,
}

impl IfExists {
    /// Parse a strategy name (case-insensitive). Recognised: `error`,
    /// `append`, `truncate`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "append" => Some(Self::Append),
            "truncate" => Some(Self::Truncate),
            _ => None,
        }
    }
}

/// Source side of a copy: a whole table or an arbitrary SELECT.
#[derive(Debug, Clone)]
pub enum CopySource {
    /// Copy a whole table. Generates `SELECT * FROM <table>` against
    /// the source.
    Table(String),
    /// Copy the result of an arbitrary SELECT into the named target
    /// table. The query must be a single SELECT — paging requires it.
    Query { sql: String, into: String },
}

impl CopySource {
    /// Returns the target table name (whether sourced from `Table` or `into`).
    pub fn target_table(&self) -> &str {
        match self {
            Self::Table(t) => t,
            Self::Query { into, .. } => into,
        }
    }

    fn source_sql(&self, src_backend: Backend) -> String {
        match self {
            Self::Table(t) => format!("SELECT * FROM {}", quote_identifier(t, src_backend)),
            Self::Query { sql, .. } => sql.clone(),
        }
    }
}

/// Options for a copy operation.
pub struct CopyOptions {
    pub source: CopySource,
    /// Translate source column metadata into destination DDL and
    /// `CREATE TABLE` if the target does not exist.
    pub create_table: bool,
    /// What to do if the target table already exists with rows.
    pub if_exists: IfExists,
    /// Wrap the entire copy in a single target-side transaction.
    pub atomic: bool,
    /// How many rows per source-side page / target-side INSERT batch.
    pub batch_size: usize,
    /// Optional progress callback invoked after each batch with the
    /// running row count.
    pub progress: Option<Box<dyn Fn(usize) + Send>>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            source: CopySource::Table(String::new()),
            create_table: false,
            if_exists: IfExists::Error,
            atomic: false,
            batch_size: 1000,
            progress: None,
        }
    }
}

impl std::fmt::Debug for CopyOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopyOptions")
            .field("source", &self.source)
            .field("create_table", &self.create_table)
            .field("if_exists", &self.if_exists)
            .field("atomic", &self.atomic)
            .field("batch_size", &self.batch_size)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

/// Stream rows from `src` to `dst` per `opts`. Returns the number of
/// rows inserted into the target.
pub async fn copy_rows(
    src: &mut dyn Connection,
    src_backend: Backend,
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &CopyOptions,
) -> Result<usize, CoreError> {
    let target_table = opts.source.target_table().to_string();
    if target_table.is_empty() {
        return Err(CoreError::QueryFailed(
            "copy: target table name is empty".into(),
        ));
    }
    if opts.batch_size == 0 {
        return Err(CoreError::QueryFailed(
            "copy: batch_size must be greater than zero".into(),
        ));
    }

    let target_exists = table_exists(dst, &target_table).await?;

    if !target_exists && !opts.create_table {
        return Err(CoreError::QueryFailed(format!(
            "Target table '{target_table}' does not exist on destination. \
             Pass --create-table to create it from the source schema."
        )));
    }

    // Pre-flight conflict check: error strategy refuses if the target
    // already holds at least one row. Source is never touched in this
    // case — fail fast.
    if target_exists
        && opts.if_exists == IfExists::Error
        && table_has_rows(dst, &target_table, dst_backend).await?
    {
        return Err(CoreError::QueryFailed(format!(
            "Target table '{target_table}' already contains rows. \
             Pass --if-exists append, --if-exists truncate, or empty \
             the table first."
        )));
    }

    // First page from source — establishes the column shape.
    let source_sql = opts.source.source_sql(src_backend);
    let first_paged = crate::query_builder::apply_paging(
        &source_sql,
        Some(opts.batch_size),
        Some(0),
        src_backend,
    )?;
    let first_page = src.query(&first_paged).await?;

    if first_page.columns.is_empty() {
        return Err(CoreError::QueryFailed(
            "copy: source query returned no column metadata".into(),
        ));
    }
    let columns: Vec<ColumnInfo> = first_page.columns.clone();

    // Translate DDL when creating the target table.
    if !target_exists && opts.create_table {
        let ddl = translate_ddl(&target_table, &columns, dst_backend);
        dst.execute(&ddl).await?;
    }

    // Open the wrapping transaction. The truncate strategy and the
    // explicit --atomic flag both want one. Truncate always wraps so
    // a failed first INSERT cannot leave the target wiped + empty.
    let want_tx = opts.atomic
        || (target_exists && opts.if_exists == IfExists::Truncate);
    let tx_opened = if want_tx {
        begin_transaction(dst, dst_backend).await
    } else {
        false
    };

    let result = run_copy(
        src,
        src_backend,
        dst,
        dst_backend,
        opts,
        &source_sql,
        &target_table,
        &columns,
        target_exists,
        first_page.rows,
    )
    .await;

    if tx_opened {
        match &result {
            Ok(_) => {
                // Commit; if commit fails, surface that as the error.
                commit_transaction(dst, dst_backend).await?;
            }
            Err(_) => {
                // Best-effort rollback; ignore secondary errors.
                let _ = rollback_transaction(dst, dst_backend).await;
            }
        }
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_copy(
    src: &mut dyn Connection,
    src_backend: Backend,
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &CopyOptions,
    source_sql: &str,
    target_table: &str,
    columns: &[ColumnInfo],
    target_exists: bool,
    first_rows: Vec<Vec<Value>>,
) -> Result<usize, CoreError> {
    // Truncate before inserting if requested.
    if target_exists && opts.if_exists == IfExists::Truncate {
        let qident = quote_identifier(target_table, dst_backend);
        let sql = format!("DELETE FROM {qident}");
        dst.execute(&sql).await?;
    }

    let quoted_table = quote_identifier(target_table, dst_backend);
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier(&c.name, dst_backend))
        .collect();
    let cols_clause = quoted_cols.join(", ");

    let mut total = 0usize;
    let mut offset = 0usize;
    let first_len = first_rows.len();

    if !first_rows.is_empty() {
        insert_batch(dst, &quoted_table, &cols_clause, &first_rows, dst_backend).await?;
        total += first_len;
        offset += first_len;
        if let Some(cb) = &opts.progress {
            cb(total);
        }
    }

    // Continue paging only if the first page was full.
    if first_len >= opts.batch_size {
        loop {
            let paged = crate::query_builder::apply_paging(
                source_sql,
                Some(opts.batch_size),
                Some(offset),
                src_backend,
            )?;
            let page = src.query(&paged).await?;
            if page.rows.is_empty() {
                break;
            }
            let fetched = page.rows.len();
            insert_batch(dst, &quoted_table, &cols_clause, &page.rows, dst_backend).await?;
            total += fetched;
            offset += fetched;
            if let Some(cb) = &opts.progress {
                cb(total);
            }
            if fetched < opts.batch_size {
                break;
            }
        }
    }

    Ok(total)
}

async fn insert_batch(
    dst: &mut dyn Connection,
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
) -> Result<(), CoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    let values: Vec<String> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = row.iter().map(|v| render_value(v, dst_backend)).collect();
            format!("({})", cells.join(", "))
        })
        .collect();
    let sql = format!(
        "INSERT INTO {quoted_table} ({cols_clause}) VALUES {}",
        values.join(", ")
    );
    dst.execute(&sql).await?;
    Ok(())
}

/// Backend-aware identifier quoting:
/// - Postgres / SQLite / Oracle / MSSQL: `"name"` (ANSI; MSSQL also
///   accepts `[name]`, but ANSI quotes work with QUOTED_IDENTIFIER ON,
///   the default).
/// - MySQL: backticks. The ANSI form requires `ANSI_QUOTES` SQL_MODE
///   which ferrule does not assume.
pub(crate) fn quote_identifier(id: &str, backend: Backend) -> String {
    match backend {
        #[cfg(feature = "mysql")]
        Backend::MySql => format!("`{}`", id.replace('`', "``")),
        #[cfg(feature = "postgres")]
        Backend::Postgres => ansi_quote(id),
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => ansi_quote(id),
        #[cfg(feature = "mssql")]
        Backend::MsSql => ansi_quote(id),
        #[cfg(feature = "oracle")]
        Backend::Oracle => ansi_quote(id),
    }
}

fn ansi_quote(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

/// Translate source column metadata into a `CREATE TABLE IF NOT EXISTS`
/// statement for the destination backend.
pub fn translate_ddl(table: &str, cols: &[ColumnInfo], dst: Backend) -> String {
    let quoted_table = quote_identifier(table, dst);
    let col_defs: Vec<String> = cols
        .iter()
        .map(|c| {
            let name = quote_identifier(&c.name, dst);
            let ty = translate_type(c.type_hint, dst);
            let null_clause = if c.nullable { "" } else { " NOT NULL" };
            format!("{name} {ty}{null_clause}")
        })
        .collect();
    format!(
        "CREATE TABLE IF NOT EXISTS {quoted_table} ({})",
        col_defs.join(", ")
    )
}

/// Map a unified [`TypeHint`] to a SQL type for the destination
/// backend. The mapping favours portability over fidelity:
/// `Decimal` collapses to a `(38,10)` default on backends that need
/// precision, `Array` stores as a JSON-ish text, and `Other`/`Null`
/// fall back to the backend's "wide string" type.
pub fn translate_type(hint: TypeHint, dst: Backend) -> &'static str {
    match dst {
        #[cfg(feature = "postgres")]
        Backend::Postgres => match hint {
            TypeHint::Bool => "BOOLEAN",
            TypeHint::Int64 => "BIGINT",
            TypeHint::Float64 => "DOUBLE PRECISION",
            TypeHint::Decimal => "NUMERIC",
            TypeHint::Bytes => "BYTEA",
            TypeHint::Date => "DATE",
            TypeHint::Time => "TIME",
            TypeHint::DateTime => "TIMESTAMP",
            TypeHint::DateTimeTz => "TIMESTAMPTZ",
            TypeHint::Json | TypeHint::Array => "JSONB",
            TypeHint::Uuid => "UUID",
            TypeHint::String | TypeHint::Other | TypeHint::Null => "TEXT",
        },
        #[cfg(feature = "mysql")]
        Backend::MySql => match hint {
            TypeHint::Bool => "TINYINT(1)",
            TypeHint::Int64 => "BIGINT",
            TypeHint::Float64 => "DOUBLE",
            TypeHint::Decimal => "DECIMAL(38,10)",
            TypeHint::Bytes => "LONGBLOB",
            TypeHint::Date => "DATE",
            TypeHint::Time => "TIME",
            TypeHint::DateTime | TypeHint::DateTimeTz => "DATETIME",
            TypeHint::Json | TypeHint::Array => "JSON",
            TypeHint::Uuid => "CHAR(36)",
            TypeHint::String | TypeHint::Other | TypeHint::Null => "TEXT",
        },
        #[cfg(feature = "mssql")]
        Backend::MsSql => match hint {
            TypeHint::Bool => "BIT",
            TypeHint::Int64 => "BIGINT",
            TypeHint::Float64 => "FLOAT",
            TypeHint::Decimal => "DECIMAL(38,10)",
            TypeHint::Bytes => "VARBINARY(MAX)",
            TypeHint::Date => "DATE",
            TypeHint::Time => "TIME",
            TypeHint::DateTime => "DATETIME2",
            TypeHint::DateTimeTz => "DATETIMEOFFSET",
            TypeHint::Json | TypeHint::Array
            | TypeHint::String | TypeHint::Other | TypeHint::Null => "NVARCHAR(MAX)",
            TypeHint::Uuid => "UNIQUEIDENTIFIER",
        },
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => match hint {
            TypeHint::Bool | TypeHint::Int64 => "INTEGER",
            TypeHint::Float64 => "REAL",
            TypeHint::Decimal => "NUMERIC",
            TypeHint::Bytes => "BLOB",
            // SQLite stores everything else as TEXT under dynamic typing.
            _ => "TEXT",
        },
        #[cfg(feature = "oracle")]
        Backend::Oracle => match hint {
            TypeHint::Bool => "NUMBER(1)",
            TypeHint::Int64 => "NUMBER(19)",
            TypeHint::Float64 => "BINARY_DOUBLE",
            TypeHint::Decimal => "NUMBER",
            TypeHint::Bytes => "BLOB",
            TypeHint::Date => "DATE",
            TypeHint::Time | TypeHint::DateTime => "TIMESTAMP",
            TypeHint::DateTimeTz => "TIMESTAMP WITH TIME ZONE",
            TypeHint::Json | TypeHint::Array | TypeHint::String
            | TypeHint::Other | TypeHint::Null => "CLOB",
            TypeHint::Uuid => "RAW(16)",
        },
    }
}

async fn table_exists(conn: &mut dyn Connection, table: &str) -> Result<bool, CoreError> {
    let tables = conn.list_tables(None).await?;
    Ok(tables.iter().any(|t| t.eq_ignore_ascii_case(table)))
}

async fn table_has_rows(
    conn: &mut dyn Connection,
    table: &str,
    backend: Backend,
) -> Result<bool, CoreError> {
    let qident = quote_identifier(table, backend);
    let sql = crate::query_builder::apply_paging(
        &format!("SELECT 1 FROM {qident}"),
        Some(1),
        None,
        backend,
    )?;
    let result = conn.query(&sql).await?;
    Ok(!result.rows.is_empty())
}

/// Open a target-side transaction. Returns `true` if the BEGIN
/// succeeded, `false` if the backend rejected the statement (best-
/// effort: the caller proceeds without a wrapping transaction).
async fn begin_transaction(conn: &mut dyn Connection, backend: Backend) -> bool {
    let stmt = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "BEGIN TRANSACTION",
        // Oracle starts implicit transactions; an explicit BEGIN here
        // would parse as a PL/SQL block. Skip the statement; the
        // wrapping COMMIT at the end still terminates the implicit txn.
        #[cfg(feature = "oracle")]
        Backend::Oracle => return true,
        _ => "BEGIN",
    };
    conn.execute(stmt).await.is_ok()
}

async fn commit_transaction(
    conn: &mut dyn Connection,
    backend: Backend,
) -> Result<(), CoreError> {
    let stmt = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "COMMIT TRANSACTION",
        _ => "COMMIT",
    };
    conn.execute(stmt).await.map(|_| ())
}

async fn rollback_transaction(
    conn: &mut dyn Connection,
    backend: Backend,
) -> Result<(), CoreError> {
    let stmt = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "ROLLBACK TRANSACTION",
        _ => "ROLLBACK",
    };
    conn.execute(stmt).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ColumnInfo;

    fn col(name: &str, hint: TypeHint, nullable: bool) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            type_hint: hint,
            nullable,
        }
    }

    #[test]
    fn if_exists_parse_recognises_strategies() {
        assert_eq!(IfExists::parse("error"), Some(IfExists::Error));
        assert_eq!(IfExists::parse("APPEND"), Some(IfExists::Append));
        assert_eq!(IfExists::parse("Truncate"), Some(IfExists::Truncate));
        assert_eq!(IfExists::parse("upsert"), None);
    }

    #[test]
    fn if_exists_default_is_non_destructive() {
        assert_eq!(IfExists::default(), IfExists::Error);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn quote_identifier_sqlite_uses_ansi_quotes() {
        assert_eq!(quote_identifier("users", Backend::Sqlite), "\"users\"");
        assert_eq!(quote_identifier("a\"b", Backend::Sqlite), "\"a\"\"b\"");
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn quote_identifier_mysql_uses_backticks() {
        assert_eq!(quote_identifier("users", Backend::MySql), "`users`");
        assert_eq!(quote_identifier("a`b", Backend::MySql), "`a``b`");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn translate_type_postgres_maps_decimal_to_numeric() {
        assert_eq!(translate_type(TypeHint::Decimal, Backend::Postgres), "NUMERIC");
        assert_eq!(translate_type(TypeHint::DateTimeTz, Backend::Postgres), "TIMESTAMPTZ");
        assert_eq!(translate_type(TypeHint::Json, Backend::Postgres), "JSONB");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn translate_type_sqlite_collapses_to_storage_classes() {
        assert_eq!(translate_type(TypeHint::Bool, Backend::Sqlite), "INTEGER");
        assert_eq!(translate_type(TypeHint::Int64, Backend::Sqlite), "INTEGER");
        assert_eq!(translate_type(TypeHint::Float64, Backend::Sqlite), "REAL");
        assert_eq!(translate_type(TypeHint::Bytes, Backend::Sqlite), "BLOB");
        assert_eq!(translate_type(TypeHint::DateTime, Backend::Sqlite), "TEXT");
        assert_eq!(translate_type(TypeHint::Json, Backend::Sqlite), "TEXT");
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn translate_type_mssql_maps_string_to_nvarchar_max() {
        assert_eq!(translate_type(TypeHint::String, Backend::MsSql), "NVARCHAR(MAX)");
        assert_eq!(translate_type(TypeHint::Uuid, Backend::MsSql), "UNIQUEIDENTIFIER");
        assert_eq!(translate_type(TypeHint::Bool, Backend::MsSql), "BIT");
    }

    #[cfg(all(feature = "postgres", feature = "sqlite"))]
    #[test]
    fn translate_ddl_postgres_to_sqlite() {
        let cols = vec![
            col("id", TypeHint::Int64, false),
            col("name", TypeHint::String, true),
            col("score", TypeHint::Float64, true),
            col("active", TypeHint::Bool, true),
            col("meta", TypeHint::Json, true),
        ];
        let ddl = translate_ddl("test_users", &cols, Backend::Sqlite);
        assert_eq!(
            ddl,
            "CREATE TABLE IF NOT EXISTS \"test_users\" (\
             \"id\" INTEGER NOT NULL, \
             \"name\" TEXT, \
             \"score\" REAL, \
             \"active\" INTEGER, \
             \"meta\" TEXT)"
        );
    }

    #[cfg(all(feature = "mysql", feature = "mssql"))]
    #[test]
    fn translate_ddl_mysql_to_mssql_uses_correct_quoting_and_types() {
        let cols = vec![
            col("id", TypeHint::Int64, false),
            col("uid", TypeHint::Uuid, true),
            col("created_at", TypeHint::DateTimeTz, true),
        ];
        let ddl = translate_ddl("orders", &cols, Backend::MsSql);
        assert_eq!(
            ddl,
            "CREATE TABLE IF NOT EXISTS \"orders\" (\
             \"id\" BIGINT NOT NULL, \
             \"uid\" UNIQUEIDENTIFIER, \
             \"created_at\" DATETIMEOFFSET)"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn copy_sqlite_to_sqlite_round_trip() {
        use crate::backends::sqlite::connect as sqlite_connect;
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = sqlite_connect(&url_a, &ConnectOptions::default()).await.unwrap();
        let mut dst = sqlite_connect(&url_b, &ConnectOptions::default()).await.unwrap();

        src.execute(
            "CREATE TABLE test_users (id INTEGER, name TEXT, age INTEGER, score REAL, active INTEGER)",
        )
        .await
        .unwrap();
        src.execute("INSERT INTO test_users VALUES (1, 'Alice', 30, 99.5, 1)").await.unwrap();
        src.execute("INSERT INTO test_users VALUES (2, 'Bob', 25, 88.25, 0)").await.unwrap();
        src.execute("INSERT INTO test_users VALUES (3, 'Carol', 40, NULL, 1)").await.unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("test_users".into()),
            create_table: true,
            if_exists: IfExists::Error,
            atomic: false,
            batch_size: 2,
            progress: None,
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .await
            .expect("copy_rows");
        assert_eq!(copied, 3);

        let out = dst
            .query("SELECT id, name, age, score, active FROM test_users ORDER BY id")
            .await
            .unwrap();
        assert_eq!(out.rows.len(), 3);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "Alice"));
        assert!(matches!(&out.rows[2][3], Value::Null));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn copy_refuses_when_target_non_empty_with_default_strategy() {
        use crate::backends::sqlite::connect as sqlite_connect;
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n}-conflict.db"));
        let _ = std::fs::remove_file(&path);

        let url = DatabaseUrl::parse(&format!("sqlite://{}", path.display())).unwrap();
        let mut src = sqlite_connect(&url, &ConnectOptions::default()).await.unwrap();
        // Open a second connection (sqlite — file path is what matters).
        let mut dst = sqlite_connect(&url, &ConnectOptions::default()).await.unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)").await.unwrap();
        src.execute("INSERT INTO t VALUES (1, 'existing')").await.unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            ..Default::default()
        };
        // Same DB on both sides — target table 't' exists with one row.
        let result = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts).await;
        let err = result.expect_err("copy should refuse non-empty target by default");
        let msg = err.to_string();
        assert!(
            msg.contains("already contains rows") && msg.contains("--if-exists"),
            "unhelpful error message: {msg}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn copy_truncate_replaces_existing_rows() {
        use crate::backends::sqlite::connect as sqlite_connect;
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-trunc-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-trunc-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = sqlite_connect(&url_a, &ConnectOptions::default()).await.unwrap();
        let mut dst = sqlite_connect(&url_b, &ConnectOptions::default()).await.unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)").await.unwrap();
        dst.execute("CREATE TABLE t (id INTEGER, name TEXT)").await.unwrap();
        dst.execute("INSERT INTO t VALUES (99, 'stale')").await.unwrap();
        src.execute("INSERT INTO t VALUES (1, 'fresh-1')").await.unwrap();
        src.execute("INSERT INTO t VALUES (2, 'fresh-2')").await.unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Truncate,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .await
            .expect("copy_rows");
        assert_eq!(copied, 2);

        let out = dst.query("SELECT id, name FROM t ORDER BY id").await.unwrap();
        assert_eq!(out.rows.len(), 2);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "fresh-1"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "fresh-2"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn copy_query_with_into_and_create_table() {
        use crate::backends::sqlite::connect as sqlite_connect;
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-q-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-q-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = sqlite_connect(&url_a, &ConnectOptions::default()).await.unwrap();
        let mut dst = sqlite_connect(&url_b, &ConnectOptions::default()).await.unwrap();

        src.execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER, active INTEGER)")
            .await.unwrap();
        src.execute("INSERT INTO users VALUES (1, 'Alice', 30, 1)").await.unwrap();
        src.execute("INSERT INTO users VALUES (2, 'Bob', 25, 0)").await.unwrap();
        src.execute("INSERT INTO users VALUES (3, 'Carol', 40, 1)").await.unwrap();

        let opts = CopyOptions {
            source: CopySource::Query {
                sql: "SELECT id, name FROM users WHERE active = 1".into(),
                into: "active_users".into(),
            },
            create_table: true,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .await
            .expect("copy_rows");
        assert_eq!(copied, 2);

        let out = dst
            .query("SELECT id, name FROM active_users ORDER BY id")
            .await.unwrap();
        assert_eq!(out.rows.len(), 2);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "Alice"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "Carol"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
