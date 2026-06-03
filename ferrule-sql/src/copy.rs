//! Cross-DB row copy: stream rows from one backend's table or query
//! into another backend's table, translating types via the unified
//! [`Value`] enum.
//!
//! The default conflict policy is non-destructive — a copy into a
//! non-empty existing target table errors out before any INSERT (or
//! source SELECT) runs. Callers opt in to `Append` or `Truncate` via
//! [`IfExists`].

use crate::backend::Backend;
use crate::connection::{BulkInsert, Connection, ForeignKey};
use crate::error::SqlError;
use crate::render::render_value;
use crate::transaction::{begin_transaction, commit_transaction, rollback_transaction};
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
    /// Insert rows whose primary key does not yet exist on the target;
    /// silently skip rows whose PK is already present. PG/SQLite use
    /// `ON CONFLICT (pk) DO NOTHING`, MySQL uses `INSERT IGNORE`,
    /// MSSQL/Oracle use a `MERGE … WHEN NOT MATCHED` statement.
    ///
    /// Requires conflict columns on the destination table — either via
    /// a declared primary key, or via the `--key COL[,COL...]`
    /// override. Tables without either raise a hard error.
    Skip,
    /// Insert rows whose primary key does not yet exist; update all
    /// non-PK columns when the PK already exists. PG/SQLite use
    /// `ON CONFLICT (pk) DO UPDATE SET col = EXCLUDED.col`, MySQL
    /// uses `INSERT … ON DUPLICATE KEY UPDATE col = VALUES(col)`,
    /// MSSQL/Oracle use a full `MERGE` with both branches.
    ///
    /// Requires a declared primary key on the destination table; see
    /// [`Skip`](Self::Skip) for the no-PK behaviour.
    Upsert,
}

impl IfExists {
    /// Parse a strategy name (case-insensitive). Recognised: `error`,
    /// `append`, `truncate`, `skip`, `upsert`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "append" => Some(Self::Append),
            "truncate" => Some(Self::Truncate),
            "skip" => Some(Self::Skip),
            "upsert" => Some(Self::Upsert),
            _ => None,
        }
    }

    /// True for the two PK-driven conflict-resolution strategies.
    /// Used by `run_copy` to look up the destination PK once up front
    /// and to force the dispatcher onto the generic INSERT path (the
    /// native bulk loaders carry no conflict semantics).
    pub fn resolves_conflicts(self) -> bool {
        matches!(self, Self::Skip | Self::Upsert)
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
    /// Try the bulk path; on [`SqlError::BulkUnavailable`] emit one
    /// stderr warning and fall back to the generic path for the
    /// current batch. Any other error surfaces immediately —
    /// degrading on, e.g., a FK violation would risk double-inserts.
    Auto,
    /// Require the bulk path. If a backend returns
    /// [`SqlError::BulkUnavailable`], `copy_rows` fails with a
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

/// Wire format used by the Postgres `COPY` bulk path.
///
/// Defaults to [`Text`] for parity with PR #40 / the Phase-1
/// dispatcher. Binary is opt-in: it skips PG's text parser, which is
/// faster on `BIGINT` / `TIMESTAMPTZ` / `UUID` / `NUMERIC`-heavy
/// schemas, but is at-best break-even on `TEXT` / `JSONB` / `BYTEA`-
/// heavy ones because typed length prefixes inflate small payloads.
///
/// PG-only. Other backends ignore the field — their bulk paths are
/// already protocol-native (TDS, MySQL `LOAD DATA`, ODPI-C).
///
/// [`Text`]: CopyFormat::Text
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CopyFormat {
    /// `COPY … WITH (FORMAT TEXT)`. Tab-separated, newline-terminated;
    /// the only path before this flag existed.
    #[default]
    Text,
    /// `COPY … WITH (FORMAT BINARY)`. Streamed via
    /// `tokio_postgres::binary_copy::BinaryCopyInWriter`; per-row
    /// values are bound through their `ToSql` impls.
    Binary,
}

impl CopyFormat {
    /// Parse a format name (case-insensitive). Recognised: `text`,
    /// `binary`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "text" => Some(Self::Text),
            "binary" => Some(Self::Binary),
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
    /// When combined with [`create_table`](Self::create_table), look up
    /// the source table's declared primary key and include a matching
    /// `PRIMARY KEY (...)` clause in the emitted DDL. Default `false`
    /// preserves the v1 column-only contract. Best-effort: source
    /// tables with no declared PK fall through to the column-only DDL.
    /// Ignored in `--query` mode (no canonical source table to inspect).
    pub preserve_pk: bool,
    /// What to do if the target table already exists with rows.
    pub if_exists: IfExists,
    /// User-supplied conflict-key column list, overriding the PK
    /// auto-detection used by [`IfExists::Skip`] / [`IfExists::Upsert`].
    /// Empty = fall back to the destination's declared primary key.
    /// Validated against the source column shape during copy preflight.
    /// Ignored for non-conflict strategies (`Error`/`Append`/`Truncate`).
    pub conflict_key: Vec<String>,
    /// Wrap the entire copy in a single target-side transaction.
    pub atomic: bool,
    /// How many rows per source-side page / target-side INSERT batch.
    pub batch_size: usize,
    /// Whether to route batches through the destination backend's
    /// native bulk loader. Default [`BulkMode::Off`] preserves
    /// Phase 1 behaviour.
    pub bulk_mode: BulkMode,
    /// Wire format for the Postgres `COPY` bulk path. Other backends
    /// ignore this field. Default [`CopyFormat::Text`].
    pub copy_format: CopyFormat,
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
            preserve_pk: false,
            if_exists: IfExists::Error,
            conflict_key: Vec::new(),
            atomic: false,
            batch_size: 1000,
            bulk_mode: BulkMode::Off,
            copy_format: CopyFormat::Text,
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
            .field("preserve_pk", &self.preserve_pk)
            .field("if_exists", &self.if_exists)
            .field("conflict_key", &self.conflict_key)
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
pub fn copy_rows(
    src: &mut dyn Connection,
    src_backend: Backend,
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &CopyOptions,
) -> Result<usize, SqlError> {
    let target_table = opts.source.target_table().to_string();
    if target_table.is_empty() {
        return Err(SqlError::QueryFailed(
            "copy: target table name is empty".into(),
        ));
    }
    if opts.batch_size == 0 {
        return Err(SqlError::QueryFailed(
            "copy: batch_size must be greater than zero".into(),
        ));
    }

    let target_exists = table_exists(dst, &target_table)?;

    if !target_exists && !opts.create_table {
        return Err(SqlError::QueryFailed(format!(
            "Target table '{target_table}' does not exist on destination. \
             Pass --create-table to create it from the source schema."
        )));
    }

    // Pre-flight conflict check: error strategy refuses if the target
    // already holds at least one row. Source is never touched in this
    // case — fail fast.
    if target_exists
        && opts.if_exists == IfExists::Error
        && table_has_rows(dst, &target_table, dst_backend)?
    {
        return Err(SqlError::QueryFailed(format!(
            "Target table '{target_table}' already contains rows. \
             Pass --if-exists append, --if-exists truncate, --if-exists skip, \
             --if-exists upsert, or empty the table first."
        )));
    }

    // `--key` is a conflict-only flag: warn once on stderr if the user
    // set it alongside a strategy that doesn't resolve conflicts. Same
    // shape as the `--bulk-native` + conflict warning above.
    if !opts.conflict_key.is_empty() && !opts.if_exists.resolves_conflicts() {
        eprintln!(
            "[ferrule] copy: --key is only meaningful with --if-exists skip|upsert; \
             ignoring for --if-exists {}.",
            if_exists_name(opts.if_exists)
        );
    }

    // Conflict-key resolution for Skip/Upsert. Centralised in
    // `resolve_conflict_key` so the same code path serves the
    // destination-PK auto-detection and the `--key COL[,COL...]` user
    // override. An empty override means "fall back to PK detection."
    let pk_columns: Vec<String> =
        resolve_conflict_key(dst, &target_table, opts.if_exists, &opts.conflict_key)?;

    // Bulk loaders carry no conflict semantics (PG `COPY` ignores
    // conflicts, MSSQL bulk has no MERGE, MySQL `LOAD DATA` has its
    // own IGNORE/REPLACE keywords, Oracle Batch is straight array DML).
    // When the user opts into Skip or Upsert we force the dispatcher
    // onto the generic INSERT path so the conflict SQL is actually
    // emitted. Warn once at the top of the copy rather than per batch.
    if opts.if_exists.resolves_conflicts() && opts.bulk_mode != BulkMode::Off {
        eprintln!(
            "[ferrule] bulk: --if-exists {} requires the generic INSERT path; \
             ignoring --bulk-native for this copy.",
            if_exists_name(opts.if_exists)
        );
    }

    // First page from source — establishes the column shape.
    let source_sql = opts.source.source_sql(src_backend);
    let first_paged = crate::query_builder::apply_paging(
        &source_sql,
        Some(opts.batch_size),
        Some(0),
        src_backend,
    )?;
    let first_page = src.query(&first_paged)?;

    if first_page.columns.is_empty() {
        return Err(SqlError::QueryFailed(
            "copy: source query returned no column metadata".into(),
        ));
    }
    let columns: Vec<ColumnInfo> = first_page.columns.clone();

    // Validate that every destination-side PK column is present in the
    // source column shape. Cross-backend copies can hit case-sensitivity
    // mismatches here (Oracle returns uppercase names; PG/MySQL return
    // them as declared) — surface those as an actionable error rather
    // than letting the conflict SQL fail at execute time with a less
    // helpful driver message.
    if !pk_columns.is_empty() {
        let source_names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        for pk in &pk_columns {
            if !source_names.iter().any(|n| n == pk) {
                return Err(SqlError::QueryFailed(format!(
                    "Target PK column '{pk}' is not present in source columns \
                     {source_names:?}. Cross-backend identifier case mismatches \
                     can cause this — re-select with explicit aliases (e.g. \
                     `SELECT id AS \"{pk}\" ...`)."
                )));
            }
        }
    }

    // Translate DDL when creating the target table. With --preserve-pk
    // set, the emitted DDL also carries a PRIMARY KEY clause derived
    // from the source's declared PK (best-effort: source tables with
    // no PK still get the v1 column-only DDL).
    if !target_exists && opts.create_table {
        let preserved_pk: Vec<String> = if opts.preserve_pk {
            let src_table = match &opts.source {
                CopySource::Table(t) => t.clone(),
                // --query mode: the source isn't a single table, so
                // there's no canonical PK to lift. Skip silently.
                CopySource::Query { .. } => String::new(),
            };
            if src_table.is_empty() {
                Vec::new()
            } else {
                src.primary_key(None, &src_table).unwrap_or_default()
            }
        } else {
            Vec::new()
        };
        let ddl = translate_ddl(&target_table, &columns, dst_backend, &preserved_pk);
        dst.execute(&ddl)?;
    }

    // --atomic wraps the entire copy in one outer transaction. The
    // truncate strategy uses a separate, *short* inner transaction
    // around just the DELETE + first batch (handled inside run_copy)
    // so it does not hold locks / redo / wal for the whole copy.
    let outer_tx_opened = if opts.atomic {
        begin_transaction(dst, dst_backend)
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
        &pk_columns,
        target_exists,
        first_page.rows,
    );

    if outer_tx_opened {
        match &result {
            Ok(_) => {
                // Commit; if commit fails, surface that as the error.
                commit_transaction(dst, dst_backend)?;
            }
            Err(_) => {
                // Best-effort rollback; ignore secondary errors.
                let _ = rollback_transaction(dst, dst_backend);
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
        commit_transaction(dst, dst_backend)?;
    }

    result
}

/// Resolve the conflict-key column list for `--if-exists skip|upsert`.
///
/// Returns `Vec::new()` for non-conflict strategies. Otherwise returns
/// the caller-supplied `override_` (if non-empty), else the
/// destination's declared primary key. A conflict strategy with no
/// available key surfaces a hard error before the source SELECT runs.
///
/// Single insertion point for both PK auto-detection and the (Phase 3)
/// `--key COL[,COL...]` user override.
fn resolve_conflict_key(
    dst: &mut dyn Connection,
    target_table: &str,
    if_exists: IfExists,
    override_: &[String],
) -> Result<Vec<String>, SqlError> {
    if !if_exists.resolves_conflicts() {
        return Ok(Vec::new());
    }
    if !override_.is_empty() {
        return Ok(override_.to_vec());
    }
    let pk = dst.primary_key(None, target_table)?;
    if pk.is_empty() {
        return Err(SqlError::QueryFailed(format!(
            "Target table '{target_table}' has no declared primary key — \
             --if-exists {} requires one. Declare a PK on the destination \
             table, pass --preserve-pk when creating it, or supply \
             --key COL[,COL...] to override the conflict columns.",
            if_exists_name(if_exists)
        )));
    }
    Ok(pk)
}

/// Backends whose client driver does *not* auto-commit each
/// `execute()` call, so `copy_rows` must issue an explicit `COMMIT`
/// at the end of a successful copy (when no outer transaction is in
/// play). Currently only Oracle behaves this way; every other
/// supported backend defaults to autocommit.
pub(crate) fn backend_needs_explicit_commit(backend: Backend) -> bool {
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
fn run_copy(
    src: &mut dyn Connection,
    src_backend: Backend,
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &CopyOptions,
    source_sql: &str,
    target_table: &str,
    columns: &[ColumnInfo],
    pk_columns: &[String],
    target_exists: bool,
    first_rows: Vec<Vec<Value>>,
) -> Result<usize, SqlError> {
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
    let need_inner_tx = target_exists && opts.if_exists == IfExists::Truncate && !opts.atomic;
    let inner_tx_opened = if need_inner_tx {
        begin_transaction(dst, dst_backend)
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
        pk_columns,
        &quoted_table,
        &cols_clause,
        &first_rows,
    );

    let first_len = match prologue {
        Ok(n) => {
            if inner_tx_opened {
                // Commit the short truncate txn before continuing.
                commit_transaction(dst, dst_backend)?;
            }
            n
        }
        Err(e) => {
            if inner_tx_opened {
                let _ = rollback_transaction(dst, dst_backend);
            }
            return Err(e);
        }
    };

    if first_len > 0
        && let Some(cb) = &opts.progress
    {
        cb(first_len);
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
            let page = src.query(&paged)?;
            if page.rows.is_empty() {
                break;
            }
            let fetched = page.rows.len();
            insert_batch(
                dst,
                target_table,
                columns,
                pk_columns,
                &quoted_table,
                &cols_clause,
                &page.rows,
                dst_backend,
                opts.if_exists,
                opts.bulk_mode,
                opts.copy_format,
                opts.verbose,
            )?;
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
fn run_truncate_and_first_batch(
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &CopyOptions,
    target_exists: bool,
    target_table: &str,
    columns: &[ColumnInfo],
    pk_columns: &[String],
    quoted_table: &str,
    cols_clause: &str,
    first_rows: &[Vec<Value>],
) -> Result<usize, SqlError> {
    if target_exists && opts.if_exists == IfExists::Truncate {
        let sql = format!("DELETE FROM {quoted_table}");
        dst.execute(&sql)?;
    }
    if !first_rows.is_empty() {
        insert_batch(
            dst,
            target_table,
            columns,
            pk_columns,
            quoted_table,
            cols_clause,
            first_rows,
            dst_backend,
            opts.if_exists,
            opts.bulk_mode,
            opts.copy_format,
            opts.verbose,
        )?;
    }
    Ok(first_rows.len())
}

/// Insert `rows` into the destination, choosing between the
/// backend's native bulk loader and the generic INSERT path per
/// `bulk_mode`. The dispatcher is shared by the truncate prologue
/// (first batch) and the streaming loop, so a single copy never
/// mixes the two paths within one run.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_batch(
    dst: &mut dyn Connection,
    target_table: &str,
    columns: &[ColumnInfo],
    pk_columns: &[String],
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
    if_exists: IfExists,
    bulk_mode: BulkMode,
    copy_format: CopyFormat,
    verbose: bool,
) -> Result<(), SqlError> {
    if rows.is_empty() {
        return Ok(());
    }
    // Conflict-resolution strategies always use the generic path —
    // the bulk loaders carry no MERGE/ON CONFLICT semantics. The
    // top-of-copy warning in `copy_rows` already informed the user.
    let bulk_eligible =
        matches!(bulk_mode, BulkMode::Auto | BulkMode::On) && !if_exists.resolves_conflicts();
    if bulk_eligible {
        let target = BulkInsert {
            table: target_table,
            columns,
            rows,
            copy_format,
        };
        match dst.bulk_insert_rows(target) {
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
            Err(SqlError::BulkUnavailable(reason)) => {
                if bulk_mode == BulkMode::On {
                    return Err(SqlError::QueryFailed(format!(
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
    for sql in build_insert_sql(
        quoted_table,
        cols_clause,
        rows,
        dst_backend,
        columns,
        if_exists,
        pk_columns,
    ) {
        dst.execute(&sql)?;
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
    columns: &[ColumnInfo],
    if_exists: IfExists,
    pk_columns: &[String],
) -> Vec<String> {
    if rows.is_empty() {
        return Vec::new();
    }
    let chunk_size = per_statement_row_cap(dst_backend)
        .unwrap_or(rows.len())
        .max(1);
    rows.chunks(chunk_size)
        .map(|chunk| {
            build_one_insert(
                quoted_table,
                cols_clause,
                chunk,
                dst_backend,
                columns,
                if_exists,
                pk_columns,
            )
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_one_insert(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
    columns: &[ColumnInfo],
    if_exists: IfExists,
    pk_columns: &[String],
) -> String {
    // Conflict-resolution dispatches to a per-backend MERGE / ON
    // CONFLICT / ODKU shape. Non-conflict paths fall through to the
    // existing plain INSERT (or INSERT ALL) code below.
    if if_exists.resolves_conflicts() && !pk_columns.is_empty() {
        return build_conflict_insert(
            quoted_table,
            cols_clause,
            rows,
            dst_backend,
            columns,
            if_exists,
            pk_columns,
        );
    }
    match dst_backend {
        #[cfg(feature = "oracle")]
        Backend::Oracle => {
            let mut sql = String::from("INSERT ALL");
            for row in rows {
                let cells: Vec<String> = row.iter().map(|v| render_value(v, dst_backend)).collect();
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
                    let cells: Vec<String> =
                        row.iter().map(|v| render_value(v, dst_backend)).collect();
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

/// Build a single-statement INSERT with backend-specific conflict
/// resolution: PG/SQLite `ON CONFLICT`, MySQL `INSERT IGNORE` /
/// `ON DUPLICATE KEY UPDATE`, MSSQL/Oracle `MERGE`.
///
/// Pre-conditions enforced by the caller: `pk_columns` is non-empty,
/// `columns` matches each row's positional shape, and `if_exists` is
/// one of `Skip` / `Upsert`. The per-statement row cap (MSSQL: 1000;
/// Oracle: 250) still applies — chunking happens in `build_insert_sql`.
#[allow(clippy::too_many_arguments)]
fn build_conflict_insert(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    dst_backend: Backend,
    columns: &[ColumnInfo],
    if_exists: IfExists,
    pk_columns: &[String],
) -> String {
    // PK columns must be quoted with the destination's identifier
    // rules — emitted in WHERE / ON / RETURNING positions.
    let quoted_pks: Vec<String> = pk_columns
        .iter()
        .map(|n| quote_identifier(n, dst_backend))
        .collect();
    // Non-PK column names, used for the UPDATE SET assignment list.
    let non_pk_quoted: Vec<String> = columns
        .iter()
        .filter(|c| !pk_columns.iter().any(|pk| pk == &c.name))
        .map(|c| quote_identifier(&c.name, dst_backend))
        .collect();
    match dst_backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => build_mssql_merge(
            quoted_table,
            cols_clause,
            rows,
            columns,
            if_exists,
            &quoted_pks,
            &non_pk_quoted,
            dst_backend,
        ),
        #[cfg(feature = "oracle")]
        Backend::Oracle => build_oracle_merge(
            quoted_table,
            cols_clause,
            rows,
            columns,
            if_exists,
            &quoted_pks,
            &non_pk_quoted,
            dst_backend,
        ),
        #[cfg(feature = "mysql")]
        Backend::MySql => build_mysql_conflict(
            quoted_table,
            cols_clause,
            rows,
            if_exists,
            &non_pk_quoted,
            dst_backend,
        ),
        // Postgres + SQLite share ON CONFLICT syntax.
        _ => build_pg_sqlite_on_conflict(
            quoted_table,
            cols_clause,
            rows,
            if_exists,
            &quoted_pks,
            &non_pk_quoted,
            dst_backend,
        ),
    }
}

fn build_pg_sqlite_on_conflict(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    if_exists: IfExists,
    quoted_pks: &[String],
    non_pk_quoted: &[String],
    dst_backend: Backend,
) -> String {
    let values = render_values_vec(rows, dst_backend);
    let pk_list = quoted_pks.join(", ");
    let conflict_clause = if if_exists == IfExists::Skip || non_pk_quoted.is_empty() {
        // Upsert on a PK-only table collapses to DO NOTHING — there
        // is nothing to update.
        format!("ON CONFLICT ({pk_list}) DO NOTHING")
    } else {
        let assignments: Vec<String> = non_pk_quoted
            .iter()
            .map(|c| format!("{c} = EXCLUDED.{c}"))
            .collect();
        format!(
            "ON CONFLICT ({pk_list}) DO UPDATE SET {}",
            assignments.join(", ")
        )
    };
    format!(
        "INSERT INTO {quoted_table} ({cols_clause}) VALUES {} {conflict_clause}",
        values.join(", ")
    )
}

#[cfg(feature = "mysql")]
fn build_mysql_conflict(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    if_exists: IfExists,
    non_pk_quoted: &[String],
    dst_backend: Backend,
) -> String {
    let values = render_values_vec(rows, dst_backend);
    match if_exists {
        IfExists::Skip => format!(
            "INSERT IGNORE INTO {quoted_table} ({cols_clause}) VALUES {}",
            values.join(", ")
        ),
        IfExists::Upsert if !non_pk_quoted.is_empty() => {
            let assignments: Vec<String> = non_pk_quoted
                .iter()
                .map(|c| format!("{c} = VALUES({c})"))
                .collect();
            format!(
                "INSERT INTO {quoted_table} ({cols_clause}) VALUES {} \
                 ON DUPLICATE KEY UPDATE {}",
                values.join(", "),
                assignments.join(", ")
            )
        }
        // Upsert on a PK-only table collapses to INSERT IGNORE.
        _ => format!(
            "INSERT IGNORE INTO {quoted_table} ({cols_clause}) VALUES {}",
            values.join(", ")
        ),
    }
}

#[cfg(feature = "mssql")]
#[allow(clippy::too_many_arguments)]
fn build_mssql_merge(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    columns: &[ColumnInfo],
    if_exists: IfExists,
    quoted_pks: &[String],
    non_pk_quoted: &[String],
    dst_backend: Backend,
) -> String {
    let source_alias_cols: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier(&c.name, dst_backend))
        .collect();
    let source_alias_clause = source_alias_cols.join(", ");
    let values = render_values_vec(rows, dst_backend);
    let on_clause: Vec<String> = quoted_pks
        .iter()
        .map(|pk| format!("dst.{pk} = src.{pk}"))
        .collect();
    let insert_values: Vec<String> = source_alias_cols
        .iter()
        .map(|c| format!("src.{c}"))
        .collect();
    let mut sql = format!(
        "MERGE INTO {quoted_table} AS dst \
         USING (VALUES {}) AS src ({source_alias_clause}) \
         ON {} ",
        values.join(", "),
        on_clause.join(" AND "),
    );
    if if_exists == IfExists::Upsert && !non_pk_quoted.is_empty() {
        let assignments: Vec<String> = non_pk_quoted
            .iter()
            .map(|c| format!("{c} = src.{c}"))
            .collect();
        sql.push_str(&format!(
            "WHEN MATCHED THEN UPDATE SET {} ",
            assignments.join(", ")
        ));
    }
    sql.push_str(&format!(
        "WHEN NOT MATCHED THEN INSERT ({cols_clause}) VALUES ({});",
        insert_values.join(", ")
    ));
    sql
}

#[cfg(feature = "oracle")]
#[allow(clippy::too_many_arguments)]
fn build_oracle_merge(
    quoted_table: &str,
    cols_clause: &str,
    rows: &[Vec<Value>],
    columns: &[ColumnInfo],
    if_exists: IfExists,
    quoted_pks: &[String],
    non_pk_quoted: &[String],
    dst_backend: Backend,
) -> String {
    let source_alias_cols: Vec<String> = columns
        .iter()
        .map(|c| quote_identifier(&c.name, dst_backend))
        .collect();
    // Oracle MERGE source clauses use `SELECT ... FROM dual UNION ALL
    // ...` rather than VALUES — Oracle has no row-constructor.
    let source_rows: Vec<String> = rows
        .iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(source_alias_cols.iter())
                .map(|(v, alias)| format!("{} AS {alias}", render_value(v, dst_backend)))
                .collect();
            format!("SELECT {} FROM dual", cells.join(", "))
        })
        .collect();
    let on_clause: Vec<String> = quoted_pks
        .iter()
        .map(|pk| format!("dst.{pk} = src.{pk}"))
        .collect();
    let insert_values: Vec<String> = source_alias_cols
        .iter()
        .map(|c| format!("src.{c}"))
        .collect();
    let mut sql = format!(
        "MERGE INTO {quoted_table} dst \
         USING ({}) src \
         ON ({}) ",
        source_rows.join(" UNION ALL "),
        on_clause.join(" AND "),
    );
    if if_exists == IfExists::Upsert && !non_pk_quoted.is_empty() {
        let assignments: Vec<String> = non_pk_quoted
            .iter()
            .map(|c| format!("dst.{c} = src.{c}"))
            .collect();
        sql.push_str(&format!(
            "WHEN MATCHED THEN UPDATE SET {} ",
            assignments.join(", ")
        ));
    }
    sql.push_str(&format!(
        "WHEN NOT MATCHED THEN INSERT ({cols_clause}) VALUES ({})",
        insert_values.join(", ")
    ));
    sql
}

fn render_values_vec(rows: &[Vec<Value>], dst_backend: Backend) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = row.iter().map(|v| render_value(v, dst_backend)).collect();
            format!("({})", cells.join(", "))
        })
        .collect()
}

fn if_exists_name(s: IfExists) -> &'static str {
    match s {
        IfExists::Error => "error",
        IfExists::Append => "append",
        IfExists::Truncate => "truncate",
        IfExists::Skip => "skip",
        IfExists::Upsert => "upsert",
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
///
/// Public so `dump.rs` (and future schema-aware modules) can route
/// identifier quoting through a single backend-aware implementation.
pub fn quote_identifier(id: &str, backend: Backend) -> String {
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
///
/// When `pk` is non-empty, a `PRIMARY KEY (...)` clause is appended
/// after the column list — this is what `--preserve-pk` wires up.
/// Pass `&[]` for the v1 column-only DDL.
pub fn translate_ddl(table: &str, cols: &[ColumnInfo], dst: Backend, pk: &[String]) -> String {
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
    let pk_clause = if pk.is_empty() {
        String::new()
    } else {
        let quoted_pks: Vec<String> = pk.iter().map(|c| quote_identifier(c, dst)).collect();
        format!(", PRIMARY KEY ({})", quoted_pks.join(", "))
    };
    format!(
        "CREATE TABLE IF NOT EXISTS {quoted_table} ({}{pk_clause})",
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
            TypeHint::Json
            | TypeHint::Array
            | TypeHint::String
            | TypeHint::Other
            | TypeHint::Null => "NVARCHAR(MAX)",
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
            TypeHint::Json
            | TypeHint::Array
            | TypeHint::String
            | TypeHint::Other
            | TypeHint::Null => "CLOB",
            TypeHint::Uuid => "RAW(16)",
        },
    }
}

fn table_exists(conn: &mut dyn Connection, table: &str) -> Result<bool, SqlError> {
    let tables = conn.list_tables(None)?;
    Ok(tables.iter().any(|t| t.eq_ignore_ascii_case(table)))
}

fn table_has_rows(
    conn: &mut dyn Connection,
    table: &str,
    backend: Backend,
) -> Result<bool, SqlError> {
    let qident = quote_identifier(table, backend);
    let sql = crate::query_builder::apply_paging(
        &format!("SELECT 1 FROM {qident}"),
        Some(1),
        None,
        backend,
    )?;
    let result = conn.query(&sql)?;
    Ok(!result.rows.is_empty())
}

// -------------------------------------------------------------------
// Phase 3: schema-level (--all-tables) copy
// -------------------------------------------------------------------

/// Options for `copy_all_tables`.
///
/// Patterns use simple shell-style globs (`*`, `?`); matching is
/// case-sensitive against the identifier shape the source returns
/// (Oracle uppercases unquoted identifiers). Tables are included when
/// they match *at least one* `--include` (or the include list is
/// empty) and match *no* `--exclude`.
pub struct AllTablesOptions {
    /// Repeatable include patterns. Empty = include everything.
    pub include: Vec<String>,
    /// Repeatable exclude patterns. Always applied after include.
    pub exclude: Vec<String>,
    /// Carry through to the per-table `CopyOptions`. Per-table
    /// `source` and `create_table` are derived inside this module.
    pub if_exists: IfExists,
    pub atomic: bool,
    pub batch_size: usize,
    pub bulk_mode: BulkMode,
    pub copy_format: CopyFormat,
    pub verbose: bool,
    pub create_table: bool,
    /// Preserve the source PK in emitted DDL (per-table lookup) when
    /// combined with `create_table`. See
    /// [`CopyOptions::preserve_pk`](CopyOptions::preserve_pk).
    pub preserve_pk: bool,
    /// User-supplied conflict-key columns applied to every table.
    /// Use sparingly with `--all-tables` — a single column list rarely
    /// makes sense across many tables. Empty = per-table PK detection.
    pub conflict_key: Vec<String>,
    /// If true, ignore cycle errors from `topo_sort` and copy in a
    /// deterministic-but-arbitrary order. The user must understand
    /// FK violations may surface as driver errors on first insert.
    pub no_fk_check: bool,
}

impl Default for AllTablesOptions {
    fn default() -> Self {
        Self {
            include: Vec::new(),
            exclude: Vec::new(),
            if_exists: IfExists::Error,
            atomic: false,
            batch_size: 1000,
            bulk_mode: BulkMode::Off,
            copy_format: CopyFormat::Text,
            verbose: false,
            create_table: false,
            preserve_pk: false,
            conflict_key: Vec::new(),
            no_fk_check: false,
        }
    }
}

/// Match `name` against `pattern` using shell-style `*` and `?`.
/// `*` matches zero or more characters; `?` matches exactly one.
/// Case-sensitive. All other characters match literally.
pub(crate) fn matches_glob(pattern: &str, name: &str) -> bool {
    fn helper(p: &[u8], n: &[u8]) -> bool {
        match (p.split_first(), n.split_first()) {
            (None, None) => true,
            (Some((b'*', rest)), _) => {
                // Try matching the rest at every suffix of `n`.
                if helper(rest, n) {
                    return true;
                }
                if let Some((_, ns)) = n.split_first() {
                    helper(p, ns)
                } else {
                    false
                }
            }
            (Some((b'?', rest_p)), Some((_, rest_n))) => helper(rest_p, rest_n),
            (Some((pc, rest_p)), Some((nc, rest_n))) if pc == nc => helper(rest_p, rest_n),
            _ => false,
        }
    }
    helper(pattern.as_bytes(), name.as_bytes())
}

/// Discover tables on `src` and apply include/exclude filters.
/// Returns the filtered list in `list_tables` order (alphabetical for
/// most backends). Caller is expected to feed this into [`topo_sort`]
/// before issuing copies.
pub fn discover_tables(
    src: &mut dyn Connection,
    schema: Option<&str>,
    include: &[String],
    exclude: &[String],
) -> Result<Vec<String>, SqlError> {
    let raw = src.list_tables(schema)?;
    Ok(raw
        .into_iter()
        .filter(|t| {
            if !include.is_empty() && !include.iter().any(|p| matches_glob(p, t)) {
                return false;
            }
            if exclude.iter().any(|p| matches_glob(p, t)) {
                return false;
            }
            true
        })
        .collect())
}

/// Error type for [`topo_sort`]. Carries the cycle path so callers
/// can surface it in the error message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleError {
    /// Tables that participate in at least one cycle, in
    /// alphabetical order so the message is deterministic.
    pub remaining: Vec<String>,
}

/// Topologically sort `tables` so parents are loaded before
/// children, using `fks` as the edge set. Tables referenced as
/// parents but not present in `tables` (e.g. excluded from
/// `--all-tables` selection) are dropped from the dependency graph.
///
/// Returns the load order on success, or [`CycleError`] when the
/// remaining graph after Kahn's algorithm still has nodes (i.e.
/// every remaining node sits on a cycle).
///
/// The output preserves the relative order of independent tables
/// from `tables` so successive runs against the same fixture
/// produce identical orderings.
pub fn topo_sort(tables: &[String], fks: &[ForeignKey]) -> Result<Vec<String>, CycleError> {
    use std::collections::{BTreeSet, HashMap, HashSet};

    let present: HashSet<String> = tables.iter().cloned().collect();
    let order_index: HashMap<String, usize> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| (t.clone(), i))
        .collect();
    // Map child -> set of parents that are in `tables`.
    let mut parents: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for t in tables {
        parents.entry(t.clone()).or_default();
    }
    for fk in fks {
        if !present.contains(&fk.child_table) || !present.contains(&fk.parent_table) {
            continue;
        }
        if fk.child_table == fk.parent_table {
            // Self-referential FK is not a multi-table cycle.
            continue;
        }
        if parents
            .get_mut(&fk.child_table)
            .unwrap()
            .insert(fk.parent_table.clone())
        {
            children
                .entry(fk.parent_table.clone())
                .or_default()
                .push(fk.child_table.clone());
        }
    }

    // Kahn's algorithm. Tie-break on the original `tables` order so
    // the output is deterministic across runs against the same input.
    let mut ready: Vec<String> = tables
        .iter()
        .filter(|t| parents.get(*t).is_some_and(|p| p.is_empty()))
        .cloned()
        .collect();
    let mut out: Vec<String> = Vec::with_capacity(tables.len());
    let mut emitted: HashSet<String> = HashSet::new();
    while !ready.is_empty() {
        let t = ready.remove(0);
        out.push(t.clone());
        emitted.insert(t.clone());
        if let Some(kids) = children.get(&t).cloned() {
            for kid in kids {
                if let Some(ps) = parents.get_mut(&kid) {
                    ps.remove(&t);
                    if ps.is_empty() && !emitted.contains(&kid) && !ready.contains(&kid) {
                        let kid_idx = *order_index.get(&kid).unwrap_or(&usize::MAX);
                        let insert_at = ready
                            .iter()
                            .position(|r| *order_index.get(r).unwrap_or(&usize::MAX) > kid_idx)
                            .unwrap_or(ready.len());
                        ready.insert(insert_at, kid);
                    }
                }
            }
        }
    }

    if out.len() == tables.len() {
        Ok(out)
    } else {
        let mut remaining: Vec<String> = tables
            .iter()
            .filter(|t| !emitted.contains(*t))
            .cloned()
            .collect();
        remaining.sort();
        Err(CycleError { remaining })
    }
}

/// Copy every table from `src` to `dst` in FK-respecting order.
///
/// Discovers tables via [`discover_tables`], orders them via
/// [`topo_sort`] (or falls back to discovery order under
/// `opts.no_fk_check`), then loops [`copy_rows`] per table. Per-table
/// progress is printed to stderr in `[i/N] copying <table> (<rows> rows)…`
/// shape.
///
/// Returns the total number of rows passed through across all tables.
#[allow(clippy::too_many_arguments)]
pub fn copy_all_tables(
    src: &mut dyn Connection,
    src_backend: Backend,
    dst: &mut dyn Connection,
    dst_backend: Backend,
    opts: &AllTablesOptions,
) -> Result<usize, SqlError> {
    let tables = discover_tables(src, None, &opts.include, &opts.exclude)?;
    if tables.is_empty() {
        return Err(SqlError::QueryFailed(
            "copy --all-tables: no tables matched the include/exclude filters.".into(),
        ));
    }

    let fks = src.list_foreign_keys(None)?;
    let ordered = match topo_sort(&tables, &fks) {
        Ok(o) => o,
        Err(cycle) if opts.no_fk_check => {
            eprintln!(
                "[ferrule] copy: FK cycle detected among {:?}; \
                 --no-fk-check is set, copying in discovery order.",
                cycle.remaining
            );
            tables.clone()
        }
        Err(cycle) => {
            return Err(SqlError::QueryFailed(format!(
                "copy --all-tables: foreign-key cycle prevents a strict load order \
                 (tables on the cycle: {:?}). Re-run with --no-fk-check to copy in \
                 discovery order; FK violations may then surface as driver errors.",
                cycle.remaining
            )));
        }
    };

    let total_tables = ordered.len();
    let mut total_rows = 0usize;
    for (idx, table) in ordered.iter().enumerate() {
        if opts.verbose {
            eprintln!("[ferrule] [{}/{total_tables}] copying {table}…", idx + 1);
        }
        let per_table = CopyOptions {
            source: CopySource::Table(table.clone()),
            create_table: opts.create_table,
            preserve_pk: opts.preserve_pk,
            if_exists: opts.if_exists,
            conflict_key: opts.conflict_key.clone(),
            atomic: opts.atomic,
            batch_size: opts.batch_size,
            bulk_mode: opts.bulk_mode,
            copy_format: opts.copy_format,
            verbose: opts.verbose,
            progress: None,
        };
        let n = copy_rows(src, src_backend, dst, dst_backend, &per_table)?;
        if opts.verbose {
            eprintln!("[ferrule] [{}/{total_tables}] {table}: {n} rows", idx + 1);
        }
        total_rows += n;
    }
    Ok(total_rows)
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
        assert_eq!(IfExists::parse("skip"), Some(IfExists::Skip));
        assert_eq!(IfExists::parse("UPSERT"), Some(IfExists::Upsert));
        assert_eq!(IfExists::parse("merge"), None);
    }

    #[test]
    fn if_exists_resolves_conflicts_only_for_skip_and_upsert() {
        assert!(!IfExists::Error.resolves_conflicts());
        assert!(!IfExists::Append.resolves_conflicts());
        assert!(!IfExists::Truncate.resolves_conflicts());
        assert!(IfExists::Skip.resolves_conflicts());
        assert!(IfExists::Upsert.resolves_conflicts());
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

    // The three tests below were ported verbatim from
    // `ferrule-core/src/dump.rs::tests` when the local
    // `quote_identifier(id)` helper was removed in favour of routing
    // every dump-side identifier through the backend-aware
    // [`quote_identifier`] in this module.

    #[cfg(feature = "sqlite")]
    #[test]
    fn quote_identifier_wraps_in_double_quotes() {
        assert_eq!(quote_identifier("users", Backend::Sqlite), "\"users\"");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn quote_identifier_escapes_embedded_quotes() {
        assert_eq!(quote_identifier("a\"b", Backend::Sqlite), "\"a\"\"b\"");
        assert_eq!(quote_identifier("\"\"", Backend::Sqlite), "\"\"\"\"\"\"");
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn quote_identifier_preserves_other_chars() {
        assert_eq!(
            quote_identifier("col with space", Backend::Sqlite),
            "\"col with space\""
        );
        assert_eq!(
            quote_identifier("snake_case_99", Backend::Sqlite),
            "\"snake_case_99\""
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn translate_type_postgres_maps_decimal_to_numeric() {
        assert_eq!(
            translate_type(TypeHint::Decimal, Backend::Postgres),
            "NUMERIC"
        );
        assert_eq!(
            translate_type(TypeHint::DateTimeTz, Backend::Postgres),
            "TIMESTAMPTZ"
        );
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
        assert_eq!(
            translate_type(TypeHint::String, Backend::MsSql),
            "NVARCHAR(MAX)"
        );
        assert_eq!(
            translate_type(TypeHint::Uuid, Backend::MsSql),
            "UNIQUEIDENTIFIER"
        );
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
        let ddl = translate_ddl("test_users", &cols, Backend::Sqlite, &[]);
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

    /// `translate_ddl` with a non-empty `pk` slice appends a
    /// `PRIMARY KEY (...)` clause, identifier-quoted per the
    /// destination backend's rules. This is the `--preserve-pk` path.
    #[cfg(feature = "sqlite")]
    #[test]
    fn translate_ddl_with_preserve_pk_emits_primary_key_clause() {
        let cols = vec![
            col("id", TypeHint::Int64, false),
            col("name", TypeHint::String, true),
        ];
        let ddl = translate_ddl("users", &cols, Backend::Sqlite, &["id".to_string()]);
        assert_eq!(
            ddl,
            "CREATE TABLE IF NOT EXISTS \"users\" (\
             \"id\" INTEGER NOT NULL, \
             \"name\" TEXT, PRIMARY KEY (\"id\"))"
        );
    }

    /// Composite primary key: every column in the supplied `pk` slice
    /// is identifier-quoted and emitted in order.
    #[cfg(feature = "sqlite")]
    #[test]
    fn translate_ddl_with_preserve_pk_emits_composite_primary_key() {
        let cols = vec![
            col("tenant", TypeHint::Int64, false),
            col("id", TypeHint::Int64, false),
            col("name", TypeHint::String, true),
        ];
        let ddl = translate_ddl(
            "users",
            &cols,
            Backend::Sqlite,
            &["tenant".to_string(), "id".to_string()],
        );
        assert!(
            ddl.ends_with("PRIMARY KEY (\"tenant\", \"id\"))"),
            "got: {ddl}"
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
        let ddl = translate_ddl("orders", &cols, Backend::MsSql, &[]);
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

    /// Single-column `id` schema used by the legacy multi-row INSERT
    /// tests. Conflict-SQL tests build richer fixtures inline.
    fn cols_id_only() -> Vec<ColumnInfo> {
        vec![ColumnInfo {
            name: "id".to_string(),
            type_hint: TypeHint::Int64,
            nullable: false,
        }]
    }

    #[test]
    fn build_insert_sql_empty_rows_returns_empty() {
        let cols = cols_id_only();
        let out = build_insert_sql(
            "\"t\"",
            "\"id\"",
            &[],
            default_backend_for_test(),
            &cols,
            IfExists::Append,
            &[],
        );
        assert!(out.is_empty());
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn build_insert_sql_sqlite_emits_single_multi_row_insert() {
        let cols = cols_id_only();
        let rows = vec![row_int(1), row_int(2), row_int(3)];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\"",
            &rows,
            Backend::Sqlite,
            &cols,
            IfExists::Append,
            &[],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], "INSERT INTO \"t\" (\"id\") VALUES (1), (2), (3)");
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn build_insert_sql_oracle_emits_insert_all_with_select_from_dual() {
        let cols = cols_id_only();
        let rows = vec![row_int(1), row_int(2), row_int(3)];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\"",
            &rows,
            Backend::Oracle,
            &cols,
            IfExists::Append,
            &[],
        );
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
        let cols = cols_id_only();
        let rows: Vec<Vec<Value>> = (0..2500).map(|i| row_int(i as i64)).collect();
        let out = build_insert_sql(
            "\"t\"",
            "\"id\"",
            &rows,
            Backend::MsSql,
            &cols,
            IfExists::Append,
            &[],
        );
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
        let cols = cols_id_only();
        let rows: Vec<Vec<Value>> = (0..600).map(|i| row_int(i as i64)).collect();
        let out = build_insert_sql(
            "\"t\"",
            "\"id\"",
            &rows,
            Backend::Oracle,
            &cols,
            IfExists::Append,
            &[],
        );
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

    // --- Phase 2 conflict-SQL codegen ----------------------------------

    /// Two-column (id PK, name) row shape used by the conflict tests.
    fn cols_id_name() -> Vec<ColumnInfo> {
        vec![
            ColumnInfo {
                name: "id".to_string(),
                type_hint: TypeHint::Int64,
                nullable: false,
            },
            ColumnInfo {
                name: "name".to_string(),
                type_hint: TypeHint::String,
                nullable: true,
            },
        ]
    }

    fn row_id_name(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Int64(id), Value::String(name.to_string())]
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn build_insert_sql_pg_skip_emits_on_conflict_do_nothing() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a"), row_id_name(2, "b")];
        let pk = vec!["id".to_string()];
        // SQLite shares ON CONFLICT syntax with Postgres; assert on the
        // common branch using SQLite (default-feature) backend.
        let out = build_insert_sql(
            "\"t\"",
            "\"id\", \"name\"",
            &rows,
            Backend::Sqlite,
            &cols,
            IfExists::Skip,
            &pk,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, 'a'), (2, 'b') \
             ON CONFLICT (\"id\") DO NOTHING"
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn build_insert_sql_pg_upsert_emits_excluded_assignments() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a"), row_id_name(2, "b")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\", \"name\"",
            &rows,
            Backend::Sqlite,
            &cols,
            IfExists::Upsert,
            &pk,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0],
            "INSERT INTO \"t\" (\"id\", \"name\") VALUES (1, 'a'), (2, 'b') \
             ON CONFLICT (\"id\") DO UPDATE SET \"name\" = EXCLUDED.\"name\""
        );
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn build_insert_sql_pg_upsert_pk_only_table_collapses_to_do_nothing() {
        // A PK-only table (no non-PK columns) has nothing to update;
        // Upsert collapses to ON CONFLICT DO NOTHING.
        let cols = vec![ColumnInfo {
            name: "id".to_string(),
            type_hint: TypeHint::Int64,
            nullable: false,
        }];
        let rows = vec![row_int(1), row_int(2)];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\"",
            &rows,
            Backend::Sqlite,
            &cols,
            IfExists::Upsert,
            &pk,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("ON CONFLICT (\"id\") DO NOTHING"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn build_insert_sql_pg_upsert_composite_pk_emits_full_pk_list() {
        let cols = vec![
            ColumnInfo {
                name: "a".to_string(),
                type_hint: TypeHint::Int64,
                nullable: false,
            },
            ColumnInfo {
                name: "b".to_string(),
                type_hint: TypeHint::Int64,
                nullable: false,
            },
            ColumnInfo {
                name: "v".to_string(),
                type_hint: TypeHint::String,
                nullable: true,
            },
        ];
        let rows = vec![vec![
            Value::Int64(1),
            Value::Int64(2),
            Value::String("x".into()),
        ]];
        let pk = vec!["a".to_string(), "b".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"a\", \"b\", \"v\"",
            &rows,
            Backend::Sqlite,
            &cols,
            IfExists::Upsert,
            &pk,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("ON CONFLICT (\"a\", \"b\") DO UPDATE SET \"v\" = EXCLUDED.\"v\""));
        // Crucially: PK columns ('a', 'b') must NOT appear in the SET list.
        assert!(!out[0].contains("\"a\" = EXCLUDED.\"a\""));
        assert!(!out[0].contains("\"b\" = EXCLUDED.\"b\""));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn build_insert_sql_mysql_skip_emits_insert_ignore() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "`t`",
            "`id`, `name`",
            &rows,
            Backend::MySql,
            &cols,
            IfExists::Skip,
            &pk,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("INSERT IGNORE INTO `t`"));
    }

    #[cfg(feature = "mysql")]
    #[test]
    fn build_insert_sql_mysql_upsert_emits_on_duplicate_key_update() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "`t`",
            "`id`, `name`",
            &rows,
            Backend::MySql,
            &cols,
            IfExists::Upsert,
            &pk,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"));
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn build_insert_sql_mssql_skip_emits_merge_when_not_matched() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\", \"name\"",
            &rows,
            Backend::MsSql,
            &cols,
            IfExists::Skip,
            &pk,
        );
        assert_eq!(out.len(), 1);
        let sql = &out[0];
        assert!(sql.starts_with("MERGE INTO \"t\" AS dst USING (VALUES "));
        assert!(sql.contains("ON dst.\"id\" = src.\"id\""));
        assert!(sql.contains("WHEN NOT MATCHED THEN INSERT"));
        // Skip means no UPDATE branch.
        assert!(!sql.contains("WHEN MATCHED"));
        assert!(sql.ends_with(';'));
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn build_insert_sql_mssql_upsert_emits_full_merge() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\", \"name\"",
            &rows,
            Backend::MsSql,
            &cols,
            IfExists::Upsert,
            &pk,
        );
        assert_eq!(out.len(), 1);
        let sql = &out[0];
        assert!(sql.contains("WHEN MATCHED THEN UPDATE SET \"name\" = src.\"name\""));
        assert!(sql.contains("WHEN NOT MATCHED THEN INSERT"));
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn build_insert_sql_oracle_skip_emits_merge_with_select_dual_source() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a"), row_id_name(2, "b")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\", \"name\"",
            &rows,
            Backend::Oracle,
            &cols,
            IfExists::Skip,
            &pk,
        );
        assert_eq!(out.len(), 1);
        let sql = &out[0];
        assert!(sql.starts_with("MERGE INTO \"t\" dst USING ("));
        assert!(sql.contains("SELECT 1 AS \"id\", 'a' AS \"name\" FROM dual"));
        assert!(sql.contains(" UNION ALL "));
        assert!(sql.contains("ON (dst.\"id\" = src.\"id\")"));
        assert!(sql.contains("WHEN NOT MATCHED THEN INSERT"));
        assert!(!sql.contains("WHEN MATCHED"));
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn build_insert_sql_oracle_upsert_includes_update_branch() {
        let cols = cols_id_name();
        let rows = vec![row_id_name(1, "a")];
        let pk = vec!["id".to_string()];
        let out = build_insert_sql(
            "\"t\"",
            "\"id\", \"name\"",
            &rows,
            Backend::Oracle,
            &cols,
            IfExists::Upsert,
            &pk,
        );
        let sql = &out[0];
        assert!(sql.contains("WHEN MATCHED THEN UPDATE SET dst.\"name\" = src.\"name\""));
        assert!(sql.contains("WHEN NOT MATCHED THEN INSERT"));
    }

    #[cfg(feature = "sqlite")]
    fn default_backend_for_test() -> Backend {
        Backend::Sqlite
    }
    #[cfg(all(not(feature = "sqlite"), feature = "postgres"))]
    fn default_backend_for_test() -> Backend {
        Backend::Postgres
    }
    #[cfg(all(not(feature = "sqlite"), not(feature = "postgres"), feature = "mysql"))]
    fn default_backend_for_test() -> Backend {
        Backend::MySql
    }
    #[cfg(all(
        not(feature = "sqlite"),
        not(feature = "postgres"),
        not(feature = "mysql"),
        feature = "mssql"
    ))]
    fn default_backend_for_test() -> Backend {
        Backend::MsSql
    }
    #[cfg(all(
        not(feature = "sqlite"),
        not(feature = "postgres"),
        not(feature = "mysql"),
        not(feature = "mssql"),
        feature = "oracle"
    ))]
    fn default_backend_for_test() -> Backend {
        Backend::Oracle
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_sqlite_to_sqlite_round_trip() {
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
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute(
            "CREATE TABLE test_users (id INTEGER, name TEXT, age INTEGER, score REAL, active INTEGER)",
        )
        .unwrap();
        src.execute("INSERT INTO test_users VALUES (1, 'Alice', 30, 99.5, 1)")
            .unwrap();
        src.execute("INSERT INTO test_users VALUES (2, 'Bob', 25, 88.25, 0)")
            .unwrap();
        src.execute("INSERT INTO test_users VALUES (3, 'Carol', 40, NULL, 1)")
            .unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("test_users".into()),
            create_table: true,
            preserve_pk: false,
            if_exists: IfExists::Error,
            conflict_key: Vec::new(),
            atomic: false,
            batch_size: 2,
            bulk_mode: BulkMode::Off,
            copy_format: CopyFormat::Text,
            verbose: false,
            progress: None,
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        assert_eq!(copied, 3);

        let out = dst
            .query("SELECT id, name, age, score, active FROM test_users ORDER BY id")
            .unwrap();
        assert_eq!(out.rows.len(), 3);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "Alice"));
        assert!(matches!(&out.rows[2][3], Value::Null));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_refuses_when_target_non_empty_with_default_strategy() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n}-conflict.db"));
        let _ = std::fs::remove_file(&path);

        let url = DatabaseUrl::parse(&format!("sqlite://{}", path.display())).unwrap();
        let mut src = crate::connect(&url, &ConnectOptions::default(), None).unwrap();
        // Open a second connection (sqlite — file path is what matters).
        let mut dst = crate::connect(&url, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'existing')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            ..Default::default()
        };
        // Same DB on both sides — target table 't' exists with one row.
        let result = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts);
        let err = result.expect_err("copy should refuse non-empty target by default");
        let msg = err.to_string();
        assert!(
            msg.contains("already contains rows") && msg.contains("--if-exists"),
            "unhelpful error message: {msg}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_truncate_replaces_existing_rows() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-trunc-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-trunc-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        dst.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        dst.execute("INSERT INTO t VALUES (99, 'stale')").unwrap();
        src.execute("INSERT INTO t VALUES (1, 'fresh-1')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'fresh-2')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Truncate,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        assert_eq!(copied, 2);

        let out = dst.query("SELECT id, name FROM t ORDER BY id").unwrap();
        assert_eq!(out.rows.len(), 2);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "fresh-1"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "fresh-2"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_query_with_into_and_create_table() {
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
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE users (id INTEGER, name TEXT, age INTEGER, active INTEGER)")
            .unwrap();
        src.execute("INSERT INTO users VALUES (1, 'Alice', 30, 1)")
            .unwrap();
        src.execute("INSERT INTO users VALUES (2, 'Bob', 25, 0)")
            .unwrap();
        src.execute("INSERT INTO users VALUES (3, 'Carol', 40, 1)")
            .unwrap();

        let opts = CopyOptions {
            source: CopySource::Query {
                sql: "SELECT id, name FROM users WHERE active = 1".into(),
                into: "active_users".into(),
            },
            create_table: true,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        assert_eq!(copied, 2);

        let out = dst
            .query("SELECT id, name FROM active_users ORDER BY id")
            .unwrap();
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
        use crate::error::SqlError;
        use async_trait::async_trait;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

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

        impl Connection for TrackingDst {
            fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError> {
                self.inner.execute(sql)
            }
            fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError> {
                self.inner.query(sql)
            }
            fn query_cursor(
                &mut self,
                sql: &str,
            ) -> Result<crate::stream::RowCursor<'_>, SqlError> {
                self.inner.query_cursor(sql)
            }
            fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, SqlError> {
                self.inner.execute_multi(sql)
            }
            fn ping(&mut self) -> Result<(), SqlError> {
                self.inner.ping()
            }
            fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, SqlError> {
                self.inner.list_tables(schema)
            }
            fn describe_table(
                &mut self,
                schema: Option<&str>,
                table: &str,
            ) -> Result<QueryResult, SqlError> {
                self.inner.describe_table(schema, table)
            }
            fn primary_key(
                &mut self,
                schema: Option<&str>,
                table: &str,
            ) -> Result<Vec<String>, SqlError> {
                self.inner.primary_key(schema, table)
            }
            fn list_foreign_keys(
                &mut self,
                schema: Option<&str>,
            ) -> Result<Vec<crate::ForeignKey>, SqlError> {
                self.inner.list_foreign_keys(schema)
            }
            fn bulk_insert_rows(&mut self, _target: BulkInsert<'_>) -> Result<usize, SqlError> {
                self.bulk_calls.fetch_add(1, Ordering::SeqCst);
                match self.behaviour {
                    BulkBehaviour::PanicIfCalled => {
                        panic!("bulk_insert_rows was invoked under BulkMode::Off");
                    }
                    BulkBehaviour::AlwaysUnavailable => Err(SqlError::BulkUnavailable(
                        "test wrapper: bulk path forced unavailable".into(),
                    )),
                }
            }
        }
    }

    #[cfg(feature = "sqlite")]
    fn seed_pair_for_dispatcher_test(
        tag: &str,
    ) -> (
        Box<dyn crate::connection::Connection>,
        Box<dyn crate::connection::Connection>,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-{tag}-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-{tag}-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        src.execute("INSERT INTO t VALUES (3, 'c')").unwrap();

        (src, dst, path_a, path_b)
    }

    /// Off mode must never call the destination's bulk path. The
    /// wrapper panics if it does.
    #[cfg(feature = "sqlite")]
    #[test]
    fn dispatcher_off_never_invokes_bulk_path() {
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (src, dst_inner, path_a, path_b) = seed_pair_for_dispatcher_test("off");
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
        let copied = copy_rows(
            src.as_mut(),
            Backend::Sqlite,
            &mut dst,
            Backend::Sqlite,
            &opts,
        )
        .expect("copy_rows");
        assert_eq!(copied, 3);
        assert_eq!(bulk_calls.load(Ordering::SeqCst), 0);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// Auto mode tries the bulk path; on BulkUnavailable it falls
    /// back per batch and the rows still land via INSERT.
    #[cfg(feature = "sqlite")]
    #[test]
    fn dispatcher_auto_falls_back_on_bulk_unavailable() {
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (src, dst_inner, path_a, path_b) = seed_pair_for_dispatcher_test("auto");
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
        let copied = copy_rows(
            src.as_mut(),
            Backend::Sqlite,
            &mut dst,
            Backend::Sqlite,
            &opts,
        )
        .expect("copy_rows");
        assert_eq!(copied, 3);
        // Both batches attempted the bulk path before falling back.
        assert_eq!(bulk_calls.load(Ordering::SeqCst), 2);
        // Rows landed via the generic INSERT path.
        let out = dst
            .inner
            .query("SELECT id, name FROM t ORDER BY id")
            .unwrap();
        assert_eq!(out.rows.len(), 3);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// On mode does not fall back: BulkUnavailable becomes a hard
    /// error with `--bulk-native` mentioned in the message.
    #[cfg(feature = "sqlite")]
    #[test]
    fn dispatcher_on_errors_when_bulk_unavailable() {
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (src, dst_inner, path_a, path_b) = seed_pair_for_dispatcher_test("on");
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
        let result = copy_rows(
            src.as_mut(),
            Backend::Sqlite,
            &mut dst,
            Backend::Sqlite,
            &opts,
        );
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

    /// SQLite end-to-end for `--if-exists skip`: rows whose PK already
    /// exists are silently dropped; new rows land; existing values on
    /// conflicting rows are preserved.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_skip_preserves_existing_rows() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-skip-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-skip-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        dst.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        // Destination has id=1 with the "old" value; copy will see
        // id=1 'new-1' from src and id=2 'src-only' (no dest match).
        dst.execute("INSERT INTO t VALUES (1, 'kept')").unwrap();
        src.execute("INSERT INTO t VALUES (1, 'new-1')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'src-only')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Skip,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        // `copied` reports rows passed through the dispatcher, not
        // rows landed. The destination is the source of truth for
        // the visible effect.
        assert_eq!(copied, 2);

        let out = dst.query("SELECT id, name FROM t ORDER BY id").unwrap();
        assert_eq!(out.rows.len(), 2);
        // id=1 keeps the original 'kept' value (skip), id=2 inserted.
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "kept"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "src-only"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// SQLite end-to-end for `--create-table --preserve-pk`: the
    /// destination DDL carries a `PRIMARY KEY (...)` clause derived
    /// from the source table's declared PK, so the freshly created
    /// destination is immediately usable as an `--if-exists upsert`
    /// target without a separate `--key` override.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_create_table_preserve_pk_emits_primary_key() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-pp-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-pp-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'b')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            create_table: true,
            preserve_pk: true,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        assert_eq!(copied, 2);

        // Destination has an `id` PK we can immediately upsert against.
        let pk = dst.primary_key(None, "t").unwrap();
        assert_eq!(pk, vec!["id".to_string()]);

        // Round-trip: upsert a changed source row, expect the destination row to overwrite.
        src.execute("UPDATE t SET name = 'a-upd' WHERE id = 1")
            .unwrap();
        let upsert_opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Upsert,
            ..Default::default()
        };
        copy_rows(
            &mut src,
            Backend::Sqlite,
            &mut dst,
            Backend::Sqlite,
            &upsert_opts,
        )
        .expect("upsert");
        let out = dst.query("SELECT name FROM t WHERE id = 1").unwrap();
        assert!(matches!(&out.rows[0][0], Value::String(s) if s == "a-upd"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// `--preserve-pk` with a source table that has no declared PK
    /// falls through to the v1 column-only DDL (best-effort, not
    /// gated). The copy still completes successfully.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_create_table_preserve_pk_falls_through_when_source_lacks_pk() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-pp2-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-pp2-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        // No PRIMARY KEY on the source.
        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'a')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            create_table: true,
            preserve_pk: true,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        assert_eq!(copied, 1);

        // Destination created without a PK — fall-through, not failure.
        let pk = dst.primary_key(None, "t").unwrap();
        assert!(pk.is_empty(), "expected no PK; got {pk:?}");

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// SQLite end-to-end for `--if-exists upsert`: existing rows are
    /// overwritten by the source values; new rows are inserted.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_upsert_overwrites_existing_rows() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-up-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-up-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        dst.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        dst.execute("INSERT INTO t VALUES (1, 'old')").unwrap();
        src.execute("INSERT INTO t VALUES (1, 'new-1')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'src-only')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Upsert,
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows");
        assert_eq!(copied, 2);

        let out = dst.query("SELECT id, name FROM t ORDER BY id").unwrap();
        assert_eq!(out.rows.len(), 2);
        // id=1 overwritten to 'new-1' (upsert), id=2 inserted.
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "new-1"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "src-only"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// `--if-exists skip` / `upsert` against a PK-less destination
    /// must hard-error before the source is touched, pointing at the
    /// `--key` override or `--preserve-pk` for the create-table path.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_skip_without_pk_hard_errors() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-nopk-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-nopk-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        // No PRIMARY KEY — Skip/Upsert can't pick conflict columns.
        dst.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'a')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Skip,
            ..Default::default()
        };
        let err = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect_err("expected hard error for no-PK + skip");
        let msg = format!("{err}");
        assert!(
            msg.contains("no declared primary key"),
            "error should reference missing PK: {msg}"
        );
        assert!(
            msg.contains("--key"),
            "error should point at the --key override: {msg}"
        );
        assert!(
            msg.contains("--preserve-pk"),
            "error should point at --preserve-pk for create-table users: {msg}"
        );

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// `--key COL[,COL...]` lets the user supply conflict columns
    /// when the destination has no declared PK. Behaviour matches the
    /// PK-driven path: existing rows are upserted, new rows inserted.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_key_override_upserts_against_pk_less_table() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-keyup-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-keyup-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        // No PRIMARY KEY on either side; UNIQUE on (id) for the conflict.
        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        dst.execute("CREATE TABLE t (id INTEGER, name TEXT, UNIQUE(id))")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'new-1')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'src-only')").unwrap();
        dst.execute("INSERT INTO t VALUES (1, 'old')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Upsert,
            conflict_key: vec!["id".to_string()],
            ..Default::default()
        };
        let copied = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_rows with --key");
        assert_eq!(copied, 2);

        let out = dst.query("SELECT id, name FROM t ORDER BY id").unwrap();
        assert_eq!(out.rows.len(), 2);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "new-1"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "src-only"));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// `--key` naming a column that isn't in the source SELECT shape
    /// fails fast — before any INSERT runs — with an actionable error.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_key_override_unknown_column_fails_fast() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-keybad-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-keybad-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        dst.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .unwrap();
        src.execute("INSERT INTO t VALUES (1, 'a')").unwrap();

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Upsert,
            conflict_key: vec!["nonexistent".to_string()],
            ..Default::default()
        };
        let err = copy_rows(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect_err("expected error for unknown --key column");
        let msg = format!("{err}");
        assert!(
            msg.contains("nonexistent"),
            "error should name the unknown column: {msg}"
        );

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// Conflict resolution must force the dispatcher onto the generic
    /// INSERT path, even under `BulkMode::On` — the bulk loaders
    /// carry no MERGE / ON CONFLICT semantics.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_upsert_forces_generic_path_even_under_bulk_on() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use dispatcher_harness::{BulkBehaviour, TrackingDst};
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-bup-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-bup-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let raw_dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        src.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        // Seed the destination directly (before wrapping) so we keep
        // a single TrackingDst handle for the actual copy. The inner
        // PanicIfCalled wrapper would block bulk attempts during copy
        // but pass through plain execute()s — but plumbing seed DDL
        // through it adds noise. Seed via a short-lived second
        // connection on the same on-disk file instead.
        let mut seed_dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();
        seed_dst
            .execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        seed_dst.execute("INSERT INTO t VALUES (1, 'old')").unwrap();
        drop(seed_dst);
        src.execute("INSERT INTO t VALUES (1, 'new-1')").unwrap();
        src.execute("INSERT INTO t VALUES (2, 'src-only')").unwrap();

        let bulk_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let mut tracking = TrackingDst {
            inner: Box::new(raw_dst),
            bulk_calls: bulk_calls.clone(),
            behaviour: BulkBehaviour::PanicIfCalled,
        };

        let opts = CopyOptions {
            source: CopySource::Table("t".into()),
            if_exists: IfExists::Upsert,
            bulk_mode: BulkMode::On,
            ..Default::default()
        };
        let copied = copy_rows(
            &mut src,
            Backend::Sqlite,
            &mut tracking,
            Backend::Sqlite,
            &opts,
        )
        .expect("copy_rows should succeed under forced-generic path");
        assert_eq!(copied, 2);
        // PanicIfCalled would have aborted if bulk_insert_rows had
        // been invoked; assert zero invocations for belt-and-braces.
        assert_eq!(bulk_calls.load(Ordering::SeqCst), 0);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    // --- Phase 3 unit tests --------------------------------------------

    #[test]
    fn matches_glob_literal_and_wildcards() {
        assert!(matches_glob("users", "users"));
        assert!(!matches_glob("users", "Users"));
        assert!(matches_glob("*", "anything"));
        assert!(matches_glob("test_*", "test_users"));
        assert!(matches_glob("test_*", "test_orders"));
        assert!(!matches_glob("test_*", "users"));
        assert!(matches_glob("?ser", "user"));
        assert!(!matches_glob("?ser", "users"));
        assert!(matches_glob("a*b*c", "axxxbyyc"));
        assert!(matches_glob("*", ""));
        assert!(!matches_glob("nonempty", ""));
    }

    fn fk(child: &str, parent: &str) -> ForeignKey {
        ForeignKey {
            child_table: child.to_string(),
            child_columns: vec!["fk".into()],
            parent_table: parent.to_string(),
            parent_columns: vec!["id".into()],
            on_delete: None,
        }
    }

    #[test]
    fn topo_sort_simple_dag_orders_parents_first() {
        let tables: Vec<String> = ["orders", "users", "items"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // orders -> users; orders -> items.
        let fks = vec![fk("orders", "users"), fk("orders", "items")];
        let out = topo_sort(&tables, &fks).expect("ordered");
        let users_pos = out.iter().position(|t| t == "users").unwrap();
        let items_pos = out.iter().position(|t| t == "items").unwrap();
        let orders_pos = out.iter().position(|t| t == "orders").unwrap();
        assert!(users_pos < orders_pos, "users must precede orders: {out:?}");
        assert!(items_pos < orders_pos, "items must precede orders: {out:?}");
    }

    #[test]
    fn topo_sort_preserves_input_order_for_independent_tables() {
        // Three tables with no FKs — output should match input order
        // so successive runs are deterministic.
        let tables: Vec<String> = ["c", "a", "b"].iter().map(|s| s.to_string()).collect();
        let out = topo_sort(&tables, &[]).expect("ordered");
        assert_eq!(out, tables);
    }

    #[test]
    fn topo_sort_drops_edges_to_excluded_parents() {
        // `users` is excluded from `tables` — the FK orders -> users
        // should be ignored entirely, not block orders from emitting.
        let tables: Vec<String> = ["orders"].iter().map(|s| s.to_string()).collect();
        let fks = vec![fk("orders", "users")];
        let out = topo_sort(&tables, &fks).expect("ordered");
        assert_eq!(out, vec!["orders".to_string()]);
    }

    #[test]
    fn topo_sort_ignores_self_referential_fk() {
        // tree-shaped tables with a `parent_id REFERENCES tree(id)`
        // self-FK should still emit cleanly — Kahn would otherwise
        // see `tree` as having itself as an unsatisfied parent.
        let tables: Vec<String> = ["tree"].iter().map(|s| s.to_string()).collect();
        let fks = vec![fk("tree", "tree")];
        let out = topo_sort(&tables, &fks).expect("ordered");
        assert_eq!(out, vec!["tree".to_string()]);
    }

    #[test]
    fn topo_sort_reports_cycle_with_remaining_nodes_sorted() {
        let tables: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // a -> b -> c -> a, a 3-cycle.
        let fks = vec![fk("a", "b"), fk("b", "c"), fk("c", "a")];
        let err = topo_sort(&tables, &fks).expect_err("cycle expected");
        assert_eq!(
            err.remaining,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn topo_sort_cycle_does_not_block_dag_tables() {
        // Mix a 2-cycle (a <-> b) with an independent table c. c
        // should still emit; only a, b are reported as the cycle.
        let tables: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let fks = vec![fk("a", "b"), fk("b", "a")];
        let err = topo_sort(&tables, &fks).expect_err("cycle expected");
        assert_eq!(err.remaining, vec!["a".to_string(), "b".to_string()]);
    }

    /// SQLite end-to-end for `--all-tables`: parent + child tables
    /// copied in FK order; child table is created on the destination
    /// (via `--create-table`) after the parent so the FK target
    /// exists when the child rows land.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_all_tables_orders_by_fk_and_copies_everything() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-all-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-all-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        // Enable FK enforcement on the destination so the test would
        // fail if the load order were wrong (child before parent).
        dst.execute("PRAGMA foreign_keys = ON").unwrap();

        src.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        src.execute(
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, \
                                  user_id INTEGER REFERENCES users(id), \
                                  total REAL)",
        )
        .unwrap();
        src.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        src.execute("INSERT INTO orders VALUES (1, 1, 9.99), (2, 1, 4.50), (3, 2, 12.00)")
            .unwrap();

        let opts = AllTablesOptions {
            create_table: true,
            ..Default::default()
        };
        let copied = copy_all_tables(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_all_tables");
        // 2 users + 3 orders.
        assert_eq!(copied, 5);

        let u = dst.query("SELECT count(*) FROM users").unwrap();
        let o = dst.query("SELECT count(*) FROM orders").unwrap();
        assert!(matches!(&u.rows[0][0], Value::Int64(2)));
        assert!(matches!(&o.rows[0][0], Value::Int64(3)));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// `--include` / `--exclude` glob filters: only matched tables
    /// are copied; topo_sort runs over the filtered subset.
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_all_tables_respects_include_and_exclude() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-incl-src.db"));
        let path_b =
            std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-incl-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();

        // Three independent tables; include `app_*`, exclude `app_logs`.
        src.execute("CREATE TABLE app_users (id INTEGER, name TEXT)")
            .unwrap();
        src.execute("CREATE TABLE app_logs (id INTEGER, msg TEXT)")
            .unwrap();
        src.execute("CREATE TABLE other (id INTEGER)").unwrap();
        src.execute("INSERT INTO app_users VALUES (1, 'A')")
            .unwrap();
        src.execute("INSERT INTO app_logs VALUES (1, 'noise')")
            .unwrap();
        src.execute("INSERT INTO other VALUES (1)").unwrap();

        let opts = AllTablesOptions {
            include: vec!["app_*".into()],
            exclude: vec!["app_logs".into()],
            create_table: true,
            ..Default::default()
        };
        let copied = copy_all_tables(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect("copy_all_tables");
        // Only app_users (1 row) — app_logs excluded, other not in include.
        assert_eq!(copied, 1);
        let tables = dst.list_tables(None).unwrap();
        assert!(tables.contains(&"app_users".to_string()));
        assert!(!tables.contains(&"app_logs".to_string()));
        assert!(!tables.contains(&"other".to_string()));

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }

    /// FK cycle without `--no-fk-check` hard-errors with the cycle
    /// path. With `--no-fk-check` set, the copy proceeds (and may or
    /// may not succeed depending on data; here we just verify the
    /// dispatcher doesn't gate on the cycle).
    #[cfg(feature = "sqlite")]
    #[test]
    fn copy_all_tables_rejects_cycle_unless_no_fk_check() {
        use crate::connection::ConnectOptions;
        use crate::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n_a = N.fetch_add(1, Ordering::SeqCst);
        let n_b = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_a}-cyc-src.db"));
        let path_b = std::env::temp_dir().join(format!("ferrule-copy-test-{pid}-{n_b}-cyc-dst.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut src = crate::connect(&url_a, &ConnectOptions::default(), None).unwrap();
        let mut dst = crate::connect(&url_b, &ConnectOptions::default(), None).unwrap();
        // FK enforcement OFF on destination so the cyclic test data
        // can land at all.
        dst.execute("PRAGMA foreign_keys = OFF").unwrap();

        // a -> b -> a, a 2-cycle.
        src.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, b_id INTEGER REFERENCES b(id))")
            .unwrap();
        src.execute("CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a(id))")
            .unwrap();
        src.execute("INSERT INTO a VALUES (1, NULL)").unwrap();
        src.execute("INSERT INTO b VALUES (1, NULL)").unwrap();

        let opts = AllTablesOptions {
            create_table: true,
            ..Default::default()
        };
        let err = copy_all_tables(&mut src, Backend::Sqlite, &mut dst, Backend::Sqlite, &opts)
            .expect_err("cycle should hard-error");
        let msg = format!("{err}");
        assert!(msg.contains("foreign-key cycle"), "{msg}");
        assert!(msg.contains("--no-fk-check"), "{msg}");

        // Same input with --no-fk-check: should succeed (copies in
        // discovery order; FK enforcement on dst is OFF).
        let opts_relaxed = AllTablesOptions {
            create_table: true,
            no_fk_check: true,
            ..Default::default()
        };
        let copied = copy_all_tables(
            &mut src,
            Backend::Sqlite,
            &mut dst,
            Backend::Sqlite,
            &opts_relaxed,
        )
        .expect("copy_all_tables with --no-fk-check");
        assert_eq!(copied, 2);

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
