//! Cross-DB row copy: stream rows from one backend's table or query
//! into another backend's table, translating types via the unified
//! [`Value`](crate::value::Value) enum.
//!
//! The default conflict policy is non-destructive — a copy into a
//! non-empty existing target table errors out before any INSERT (or
//! source SELECT) runs. Callers opt in to `Append` or `Truncate` via
//! [`IfExists`].

use crate::backend::Backend;
use crate::connection::{BulkInsert, Connection};
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

/// Whether `copy_rows` should route INSERT batches through the
/// backend's native bulk loader.
///
/// The default is [`Off`] so v1 behaviour is identical to the
/// Phase 1 generic-INSERT path. Flipping to [`Auto`] is tracked as
/// a separate follow-up.
///
/// [`Off`]: BulkMode::Off
/// [`Auto`]: BulkMode::Auto
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BulkMode {
    /// Never use the bulk path. Every batch goes through the generic
    /// `INSERT INTO ... VALUES (..), (..)` (or backend equivalent).
    #[default]
    Off,
    /// Try the bulk path; on [`CoreError::BulkUnavailable`] emit one
    /// stderr warning and fall back to the generic path for the
    /// current batch. Any other error surfaces immediately —
    /// degrading on, e.g., a FK violation would risk double-inserts.
    Auto,
    /// Require the bulk path. If a backend returns
    /// [`CoreError::BulkUnavailable`], `copy_rows` fails with a
    /// usage-style error instead of falling back.
    On,
}

impl BulkMode {
    /// Parse a mode name (case-insensitive). Recognised: `off`,
    /// `auto`, `on`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            "on" => Some(Self::On),
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
    /// Whether to route batches through the destination backend's
    /// native bulk loader. Default [`BulkMode::Off`] preserves
    /// Phase 1 behaviour.
    pub bulk_mode: BulkMode,
    /// Whether `copy_rows` should emit per-event diagnostics on
    /// stderr (currently: a one-line "using native path" notice when
    /// the bulk path is selected, plus the standard fallback warning
    /// in [`BulkMode::Auto`]). Mirrors the CLI `--verbose` flag.
    pub verbose: bool,
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
            bulk_mode: BulkMode::Off,
            verbose: false,
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
            .field("bulk_mode", &self.bulk_mode)
            .field("verbose", &self.verbose)
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

    // --atomic wraps the entire copy in one outer transaction. The
    // truncate strategy uses a separate, *short* inner transaction
    // around just the DELETE + first batch (handled inside run_copy)
    // so it does not hold locks / redo / wal for the whole copy.
    let outer_tx_opened = if opts.atomic {
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

    if outer_tx_opened {
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
    } else if result.is_ok() && backend_needs_explicit_commit(dst_backend) {
        // L1: Oracle has no client-side autocommit (the oracle crate
        // requires an explicit `COMMIT`). The other backends auto-
        // commit each `execute()` by default. Without an explicit
        // commit here, rows inserted by run_copy() would silently
        // roll back at session close — making the function appear
        // successful but losing the data. Issue the commit when no
        // outer transaction was opened (the --atomic branch above
        // already handles its own commit).
        commit_transaction(dst, dst_backend).await?;
    }

    result
}

/// Backends whose client driver does *not* auto-commit each
/// `execute()` call, so `copy_rows` must issue an explicit `COMMIT`
/// at the end of a successful copy (when no outer transaction is in
/// play). Currently only Oracle behaves this way; every other
/// supported backend defaults to autocommit.
fn backend_needs_explicit_commit(backend: Backend) -> bool {
    #[cfg(feature = "oracle")]
    {
        if matches!(backend, Backend::Oracle) {
            return true;
        }
    }
    let _ = backend;
    false
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
    let quoted_table = quote_identifier(target_table, dst_backend);
    let quoted_cols: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier(&c.name, dst_backend))
        .collect();
    let cols_clause = quoted_cols.join(", ");

    // Inner mini-transaction wraps DELETE + first batch when the
    // truncate strategy is in play AND we're not already inside the
    // outer --atomic transaction. The inner txn commits as soon as the
    // first batch lands, so subsequent batches do not hold locks.
    let need_inner_tx = target_exists
        && opts.if_exists == IfExists::Truncate
        && !opts.atomic;
    let inner_tx_opened = if need_inner_tx {
        begin_transaction(dst, dst_backend).await
    } else {
        false
    };

    // Prologue: DELETE (if truncate) + first INSERT batch.
    let prologue = run_truncate_and_first_batch(
        dst,
        dst_backend,
        opts,
        target_exists,
        target_table,
        columns,
        &quoted_table,
        &cols_clause,
        &first_rows,
    )
    .await;

    let first_len = match prologue {
        Ok(n) => {
            if inner_tx_opened {
                // Commit the short truncate txn before continuing.
                commit_transaction(dst, dst_backend).await?;
            }
            n
        }
        Err(e) => {
            if inner_tx_opened {
                let _ = rollback_transaction(dst, dst_backend).await;
            }
            return Err(e);
        }
    };

    if first_len > 0 {
        if let Some(cb) = &opts.progress {
            cb(first_len);
        }
    }

    let mut total = first_len;
    let mut offset = first_len;

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
            insert_batch(
                dst,
                target_table,
                columns,
                &quoted_table,
                &cols_clause,
                &page.rows,
                dst_backend,
                opts.bulk_mode,
                opts.verbose,
            )
            .await?;
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

#[allow(clippy::too_many_arguments)]
async fn run_truncate_and_first_batch(
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &CopyOptions,
    target_exists: bool,
    target_table: &str,
    columns: &[ColumnInfo],
    quoted_table: &str,
    cols_clause: &str,
    first_rows: &[Vec<Value>],
) -> Result<usize, CoreError> {
    if target_exists && opts.if_exists == IfExists::Truncate {
        let sql = format!("DELETE FROM {quoted_table}");
        dst.execute(&sql).await?;
    }
    if !first_rows.is_empty() {
        insert_batch(
            dst,
            target_table,
            columns,
            quoted_table,
            cols_clause,
            first_rows,
            dst_backend,
            opts.bulk_mode,
            opts.verbose,
        )
        .await?;
    }
    Ok(first_rows.len())
}

/// Insert `rows` into the destination, choosing between the
/// backend's native bulk loader and the generic INSERT path per
/// `bulk_mode`. The dispatcher is shared by the truncate prologue
/// (first batch) and the streaming loop, so a single copy never
/// mixes the two paths within one run.
#[allow(clippy::too_many_arguments)]
async fn insert_batch(
    dst: &mut dyn Connection,
    target_table: &str,
    columns: &[ColumnInfo],
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
    bulk_mode: BulkMode,
    verbose: bool,
) -> Result<(), CoreError> {
    if rows.is_empty() {
        return Ok(());
    }
    if matches!(bulk_mode, BulkMode::Auto | BulkMode::On) {
        let target = BulkInsert {
            table: target_table,
            columns,
            rows,
        };
        match dst.bulk_insert_rows(target).await {
            Ok(_) => {
                if verbose {
                    eprintln!(
                        "[ferrule] bulk: inserted {} rows via {} native path",
                        rows.len(),
                        dst_backend.name()
                    );
                }
                return Ok(());
            }
            Err(CoreError::BulkUnavailable(reason)) => {
                if bulk_mode == BulkMode::On {
                    return Err(CoreError::QueryFailed(format!(
                        "--bulk-native=on but {} bulk path unavailable: {reason}. \
                         Re-run with --bulk-native=auto to fall back to generic INSERT, \
                         or --bulk-native=off to disable bulk entirely.",
                        dst_backend.name()
                    )));
                }
                // Auto: warn once per batch, then fall through. Per-batch
                // is intentional — multi-batch copies on the same broken
                // path would otherwise silently lose context.
                eprintln!(
                    "[ferrule] bulk: {} path unavailable: {reason}; using generic INSERT",
                    dst_backend.name()
                );
            }
            Err(other) => return Err(other),
        }
    }
    for sql in build_insert_sql(quoted_table, cols_clause, rows, dst_backend) {
        dst.execute(&sql).await?;
    }
    Ok(())
}

/// Build one or more INSERT statements for `rows`, chunking and using
/// backend-appropriate syntax. Returns an empty vec for empty input.
///
/// - Oracle uses `INSERT ALL ... SELECT 1 FROM DUAL` (multi-row
///   `VALUES (..), (..)` is not valid Oracle syntax).
/// - MSSQL caps each statement at 1000 rows (T-SQL row-constructor
///   limit; error 10738 above that).
/// - Oracle caps each statement at 250 rows (defensive — practical
///   SQL-text-size and `ORA-01795` ceilings tighten quickly past a
///   few hundred rows of literal values).
/// - Postgres / MySQL / SQLite emit a single statement.
pub(crate) fn build_insert_sql(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let chunk_size = per_statement_row_cap(dst_backend)
        .unwrap_or(rows.len())
        .max(1);
    rows.chunks(chunk_size)
        .map(|chunk| build_one_insert(quoted_table, cols_clause, chunk, dst_backend))
        .collect()
}

fn build_one_insert(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
) -> String {
    match dst_backend {
        #[cfg(feature = "oracle")]
        Backend::Oracle => {
            let mut sql = String::from("INSERT ALL");
            for row in rows {
                let cells: Vec<String> = row
                    .iter()
                    .map(|v| render_value(v, dst_backend))
                    .collect();
                sql.push_str(&format!(
                    " INTO {quoted_table} ({cols_clause}) VALUES ({})",
                    cells.join(", ")
                ));
            }
            sql.push_str(" SELECT 1 FROM DUAL");
            sql
        }
        _ => {
            let values: Vec<String> = rows
                .iter()
                .map(|row| {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|v| render_value(v, dst_backend))
                        .collect();
                    format!("({})", cells.join(", "))
                })
                .collect();
            format!(
                "INSERT INTO {quoted_table} ({cols_clause}) VALUES {}",
                values.join(", ")
            )
        }
    }
}

fn per_statement_row_cap(backend: Backend) -> Option<usize> {
    match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => Some(1000),
        #[cfg(feature = "oracle")]
        Backend::Oracle => Some(250),
        _ => None,
    }
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

    #[test]
    fn bulk_mode_parse_recognises_modes() {
        assert_eq!(BulkMode::parse("off"), Some(BulkMode::Off));
        assert_eq!(BulkMode::parse("Auto"), Some(BulkMode::Auto));
        assert_eq!(BulkMode::parse("ON"), Some(BulkMode::On));
        assert_eq!(BulkMode::parse("native"), None);
    }

    #[test]
    fn bulk_mode_default_is_off() {
        assert_eq!(BulkMode::default(), BulkMode::Off);
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

    fn row_int(n: i64) -> Vec<Value> {
        vec![Value::Int64(n)]
    }

    #[test]
    fn build_insert_sql_empty_rows_returns_empty() {
        let out = build_insert_sql("\"t\"", "\"id\"", &[], default_backend_for_test());
        assert!(out.is_empty());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn build_insert_sql_sqlite_emits_single_multi_row_insert() {
        let rows = vec![row_int(1), row_int(2), row_int(3)];
        let out = build_insert_sql("\"t\"", "\"id\"", &rows, Backend::Sqlite);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "INSERT INTO \"t\" (\"id\") VALUES (1), (2), (3)");
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn build_insert_sql_oracle_emits_insert_all_with_select_from_dual() {
        let rows = vec![row_int(1), row_int(2), row_int(3)];
        let out = build_insert_sql("\"t\"", "\"id\"", &rows, Backend::Oracle);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            "INSERT ALL\
             \u{0020}INTO \"t\" (\"id\") VALUES (1)\
             \u{0020}INTO \"t\" (\"id\") VALUES (2)\
             \u{0020}INTO \"t\" (\"id\") VALUES (3)\
             \u{0020}SELECT 1 FROM DUAL"
        );
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn build_insert_sql_mssql_splits_above_1000_rows() {
        let rows: Vec<Vec<Value>> = (0..2500).map(|i| row_int(i as i64)).collect();
        let out = build_insert_sql("\"t\"", "\"id\"", &rows, Backend::MsSql);
        // 2500 rows / 1000 cap = 3 chunks (1000 / 1000 / 500).
        assert_eq!(out.len(), 3);
        // Each chunk should be a single INSERT statement.
        for sql in &out {
            assert!(sql.starts_with("INSERT INTO \"t\" (\"id\") VALUES "));
        }
        // Sanity check the row-counts via comma counts: chunk 0 should
        // have 999 commas separating 1000 row tuples.
        assert_eq!(out[0].matches("), (").count(), 999);
        assert_eq!(out[1].matches("), (").count(), 999);
        assert_eq!(out[2].matches("), (").count(), 499);
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn build_insert_sql_oracle_chunks_at_250_rows() {
        let rows: Vec<Vec<Value>> = (0..600).map(|i| row_int(i as i64)).collect();
        let out = build_insert_sql("\"t\"", "\"id\"", &rows, Backend::Oracle);
        // 600 / 250 = 3 chunks (250 / 250 / 100).
        assert_eq!(out.len(), 3);
        for sql in &out {
            assert!(sql.starts_with("INSERT ALL"));
            assert!(sql.ends_with(" SELECT 1 FROM DUAL"));
        }
        // Each "INTO ... VALUES" occurrence is exactly one row.
        assert_eq!(out[0].matches(" INTO ").count(), 250);
        assert_eq!(out[1].matches(" INTO ").count(), 250);
        assert_eq!(out[2].matches(" INTO ").count(), 100);
    }

    #[cfg(feature = "sqlite")]
    fn default_backend_for_test() -> Backend { Backend::Sqlite }
    #[cfg(all(not(feature = "sqlite"), feature = "postgres"))]
    fn default_backend_for_test() -> Backend { Backend::Postgres }
    #[cfg(all(not(feature = "sqlite"), not(feature = "postgres"), feature = "mysql"))]
    fn default_backend_for_test() -> Backend { Backend::MySql }
    #[cfg(all(not(feature = "sqlite"), not(feature = "postgres"), not(feature = "mysql"), feature = "mssql"))]
    fn default_backend_for_test() -> Backend { Backend::MsSql }
    #[cfg(all(not(feature = "sqlite"), not(feature = "postgres"), not(feature = "mysql"), not(feature = "mssql"), feature = "oracle"))]
    fn default_backend_for_test() -> Backend { Backend::Oracle }

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
            bulk_mode: BulkMode::Off,
            verbose: false,
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

    /// Dispatcher harness: wraps a real [`Connection`] but intercepts
    /// `bulk_insert_rows` so individual tests can observe how the
    /// `copy_rows` dispatcher routes batches per [`BulkMode`].
    #[cfg(feature = "sqlite")]
    mod dispatcher_harness {
        use crate::connection::{
            BulkInsert, Connection, ExecutionSummary, QueryResult, StatementResult,
        };
        use crate::error::CoreError;
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        /// What `bulk_insert_rows` should do on the destination wrapper.
        pub enum BulkBehaviour {
            /// Test asserts the bulk path is never invoked.
            PanicIfCalled,
            /// Always return `BulkUnavailable`.
            AlwaysUnavailable,
        }

        pub struct TrackingDst {
            pub inner: Box<dyn Connection>,
            pub bulk_calls: Arc<AtomicUsize>,
            pub behaviour: BulkBehaviour,
        }

        #[async_trait]
        impl Connection for TrackingDst {
            async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, CoreError> {
                self.inner.execute(sql).await
            }
            async fn query(&mut self, sql: &str) -> Result<QueryResult, CoreError> {
                self.inner.query(sql).await
            }
            async fn execute_multi(
                &mut self,
                sql: &str,
            ) -> Result<Vec<StatementResult>, CoreError> {
                self.inner.execute_multi(sql).await
            }
            async fn ping(&mut self) -> Result<(), CoreError> {
                self.inner.ping().await
            }
            async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, CoreError> {
                self.inner.list_tables(schema).await
            }
            async fn describe_table(
                &mut self,
                schema: Option<&str>,
                table: &str,
            ) -> Result<QueryResult, CoreError> {
                self.inner.describe_table(schema, table).await
            }
            async fn primary_key(
                &mut self,
                schema: Option<&str>,
                table: &str,
            ) -> Result<Vec<String>, CoreError> {
                self.inner.primary_key(schema, table).await
            }
            async fn list_foreign_keys(
                &mut self,
                schema: Option<&str>,
            ) -> Result<Vec<crate::ForeignKey>, CoreError> {
                self.inner.list_foreign_keys(schema).await
            }
            async fn bulk_insert_rows(
                &mut self,
                _target: BulkInsert<'_>,
            ) -> Result<usize, CoreError> {
                self.bulk_calls.fetch_add(1, Ordering::SeqCst);
                match self.behaviour {
                    BulkBehaviour::PanicIfCalled => {
                        panic!("bulk_insert_rows was invoked under BulkMode::Off");
                    }
                    BulkBehaviour::AlwaysUnavailable => Err(CoreError::BulkUnavailable(
                        "test wrapper: bulk path forced unavailable".into(),
                    )),
                }
            }
        }
    }

    #[cfg(feature = "sqlite")]
    async fn seed_pair_for_dispatcher_test(
        tag: &str,
    ) -> (
        Box<dyn crate::connection::Connection>,
        Box<dyn crate::connection::Connection>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        use crate::backends::sqlite::connect as sqlite_connect;
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir()
            .join(format!("ferrule-copy-test-{pid}-{n_a}-{tag}-src.db"));
        let path_b = std::env::temp_dir()
            .join(format!("ferrule-copy-test-{pid}-{n_b}-{tag}-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = sqlite_connect(&url_a, &ConnectOptions::default()).await.unwrap();
        let dst = sqlite_connect(&url_b, &ConnectOptions::default()).await.unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)").await.unwrap();
        src.execute("INSERT INTO t VALUES (1, 'a')").await.unwrap();
        src.execute("INSERT INTO t VALUES (2, 'b')").await.unwrap();
        src.execute("INSERT INTO t VALUES (3, 'c')").await.unwrap();

        (Box::new(src), Box::new(dst), path_a, path_b)
    }

    /// Off mode must never call the destination's bulk path. The
    /// wrapper panics if it does.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn dispatcher_off_never_invokes_bulk_path() {
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (src, dst_inner, path_a, path_b) =
            seed_pair_for_dispatcher_test("off").await;
        let bulk_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mut src = src;
        let mut dst = TrackingDst {
            inner: dst_inner,
            bulk_calls: bulk_calls.clone(),
            behaviour: BulkBehaviour::PanicIfCalled,
        };

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            create_table: true,
            bulk_mode: BulkMode::Off,
            ..Default::default()
        };
        let copied =
            copy_rows(src.as_mut(), Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
                .await
                .expect("copy_rows");
        assert_eq!(copied, 3);
        assert_eq!(bulk_calls.load(Ordering::SeqCst), 0);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// Auto mode tries the bulk path; on BulkUnavailable it falls
    /// back per batch and the rows still land via INSERT.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn dispatcher_auto_falls_back_on_bulk_unavailable() {
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (src, dst_inner, path_a, path_b) =
            seed_pair_for_dispatcher_test("auto").await;
        let bulk_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mut src = src;
        let mut dst = TrackingDst {
            inner: dst_inner,
            bulk_calls: bulk_calls.clone(),
            behaviour: BulkBehaviour::AlwaysUnavailable,
        };

        // batch_size=2 against 3 source rows means: 1 prologue
        // (2 rows) + 1 streaming batch (1 row) = 2 dispatcher calls.
        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            create_table: true,
            batch_size: 2,
            bulk_mode: BulkMode::Auto,
            ..Default::default()
        };
        let copied =
            copy_rows(src.as_mut(), Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
                .await
                .expect("copy_rows");
        assert_eq!(copied, 3);
        // Both batches attempted the bulk path before falling back.
        assert_eq!(bulk_calls.load(Ordering::SeqCst), 2);
        // Rows landed via the generic INSERT path.
        let out = dst
            .inner
            .query("SELECT id, name FROM t ORDER BY id")
            .await
            .unwrap();
        assert_eq!(out.rows.len(), 3);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// On mode does not fall back: BulkUnavailable becomes a hard
    /// error with `--bulk-native` mentioned in the message.
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn dispatcher_on_errors_when_bulk_unavailable() {
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (src, dst_inner, path_a, path_b) =
            seed_pair_for_dispatcher_test("on").await;
        let bulk_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mut src = src;
        let mut dst = TrackingDst {
            inner: dst_inner,
            bulk_calls: bulk_calls.clone(),
            behaviour: BulkBehaviour::AlwaysUnavailable,
        };

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            create_table: true,
            bulk_mode: BulkMode::On,
            ..Default::default()
        };
        let result =
            copy_rows(src.as_mut(), Backend::Sqlite, &mut dst, Backend::Sqlite, &opts).await;
        let err = result.expect_err("copy should fail when bulk path unavailable in On mode");
        let msg = err.to_string();
        assert!(
            msg.contains("--bulk-native"),
            "error should mention --bulk-native: {msg}"
        );
        // Exactly one bulk attempt before the hard error.
        assert_eq!(bulk_calls.load(Ordering::SeqCst), 1);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
