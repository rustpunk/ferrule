//! Embeddable batched write path with structured partial-failure
//! reporting.
//!
//! This is the write counterpart to the streaming read cursor: a host
//! embedding `ferrule-sql` as an *output sink* pushes rows in and gets
//! back a structured [`WriteReport`] describing exactly which batches /
//! rows landed and which were rejected, so it can route the rejects.
//!
//! **Reuse, not reinvention.** Every byte of SQL this module emits comes
//! from the existing copy/load machinery — [`build_insert_sql`] and the
//! per-dialect conflict (ON CONFLICT / MERGE / ODKU) builders, the
//! [`insert_batch`] bulk-vs-generic dispatcher, [`quote_identifier`],
//! [`render_value`](crate::render::render_value), and the
//! [`transaction`](crate::transaction) helpers. The write path only adds
//! batching, back-pressure, the atomic-boundary policy, and the
//! structured result.
//!
//! **Bounded, back-pressured batches.** Rows are consumed from any
//! iterator and flushed in fixed-size batches
//! ([`WriteOptions::batch_size`]); only one batch is buffered at a time,
//! so peak memory is `O(batch_size)` regardless of how many rows the
//! source iterator yields. Pair this with the streaming
//! [`RowCursor`](crate::RowCursor) on the read side for an end-to-end
//! bounded-memory pipe.
//!
//! [`build_insert_sql`]: crate::copy
//! [`insert_batch`]: crate::copy
//! [`quote_identifier`]: crate::copy::quote_identifier

use crate::backend::Backend;
use crate::connection::Connection;
use crate::copy::{
    BulkMode, CopyFormat, IfExists, backend_needs_explicit_commit, insert_batch, quote_identifier,
};
use crate::error::SqlError;
use crate::transaction::{begin_transaction, commit_transaction, rollback_transaction};
use crate::value::{ColumnInfo, Row};

/// Default rows per write batch. Caps the in-flight buffer so a write of
/// an unbounded row stream stays `O(batch_size)` in memory.
pub const DEFAULT_WRITE_BATCH: usize = 1000;

/// Host-facing conflict semantics for a batched write.
///
/// Maps onto the same per-dialect SQL the cross-DB copy path uses
/// ([`IfExists`]); exposed as its own enum so the write API names the
/// *write* intent (insert / skip / upsert) rather than the copy-time
/// "what if the target exists" framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteMode {
    /// Plain `INSERT`. A primary-key / unique conflict surfaces as a
    /// driver error and rejects the batch (or, under
    /// [`WriteOptions::isolate_failures`], the offending rows).
    #[default]
    Insert,
    /// Insert rows whose key is new; silently skip rows whose key
    /// already exists (`ON CONFLICT DO NOTHING` / `INSERT IGNORE` /
    /// `MERGE … WHEN NOT MATCHED`). Requires conflict columns.
    Skip,
    /// Insert new rows; overwrite the non-key columns of rows whose key
    /// already exists (`ON CONFLICT DO UPDATE` / `ON DUPLICATE KEY
    /// UPDATE` / full `MERGE`). Requires conflict columns.
    Upsert,
}

impl WriteMode {
    /// The copy-layer [`IfExists`] strategy this write mode reuses.
    fn if_exists(self) -> IfExists {
        match self {
            WriteMode::Insert => IfExists::Append,
            WriteMode::Skip => IfExists::Skip,
            WriteMode::Upsert => IfExists::Upsert,
        }
    }

    /// True for the key-driven modes that need conflict columns.
    fn needs_key(self) -> bool {
        matches!(self, WriteMode::Skip | WriteMode::Upsert)
    }
}

/// Configuration for a batched write.
pub struct WriteOptions {
    /// Conflict semantics. Default [`WriteMode::Insert`].
    pub mode: WriteMode,
    /// Rows per batch. `0` is treated as [`DEFAULT_WRITE_BATCH`].
    pub batch_size: usize,
    /// Conflict-key columns for [`WriteMode::Skip`] / [`WriteMode::Upsert`].
    /// Must be a subset of `columns`. Empty is rejected for those modes.
    pub key_columns: Vec<String>,
    /// Route batches through the destination's native bulk loader
    /// ([`BulkMode::Auto`] / [`BulkMode::On`]). Ignored for the
    /// conflict modes, whose bulk loaders carry no MERGE semantics.
    pub bulk_mode: BulkMode,
    /// Postgres `COPY` wire format for the bulk path. Other backends
    /// ignore it.
    pub copy_format: CopyFormat,
    /// Wrap the whole write in one outer transaction: all batches commit
    /// together or roll back together. Uses the same per-backend
    /// BEGIN/COMMIT/ROLLBACK as `migrate`'s atomic apply — MSSQL adds
    /// `SET XACT_ABORT ON`; Oracle gets an explicit COMMIT. A batch
    /// failure under `atomic` rolls the whole write back and the report
    /// records the failing batch.
    pub atomic: bool,
    /// On a batch failure, retry that batch **one row at a time** to
    /// pinpoint the rejected rows (recorded as [`RejectedRow`]s) and let
    /// the good rows through. Off by default — the cheaper per-batch
    /// granularity rejects the whole failing batch. Mutually weakened by
    /// `atomic`: under one outer transaction a row probe cannot commit
    /// partial good rows, so `isolate_failures` only refines the
    /// *diagnosis*, not the commit boundary.
    pub isolate_failures: bool,
    /// Emit per-batch diagnostics on stderr (bulk path selection /
    /// fallback). Mirrors the CLI `--verbose` flag.
    pub verbose: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            mode: WriteMode::default(),
            batch_size: DEFAULT_WRITE_BATCH,
            key_columns: Vec::new(),
            bulk_mode: BulkMode::Off,
            copy_format: CopyFormat::Text,
            atomic: false,
            isolate_failures: false,
            verbose: false,
        }
    }
}

impl std::fmt::Debug for WriteOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteOptions")
            .field("mode", &self.mode)
            .field("batch_size", &self.batch_size)
            .field("key_columns", &self.key_columns)
            .field("bulk_mode", &self.bulk_mode)
            .field("copy_format", &self.copy_format)
            .field("atomic", &self.atomic)
            .field("isolate_failures", &self.isolate_failures)
            .field("verbose", &self.verbose)
            .finish()
    }
}

/// What happened to one batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOutcome {
    /// Every row in the batch was written.
    Written,
    /// The batch (or specific rows within it) was rejected; details are
    /// in the [`WriteReport`]'s `rejected_batches` / `rejected_rows`.
    Rejected,
}

/// A batch that failed to write as a unit.
///
/// `start_row` is the 0-based index (within the whole write) of the
/// first row in the batch; `row_count` its size. `error` is the driver
/// error message. When [`WriteOptions::isolate_failures`] is set, the
/// per-row breakdown is in [`WriteReport::rejected_rows`] instead.
#[derive(Debug, Clone)]
pub struct RejectedBatch {
    pub batch_index: usize,
    pub start_row: u64,
    pub row_count: usize,
    pub error: String,
}

/// A single row rejected during a [`WriteOptions::isolate_failures`]
/// probe. `row_index` is the 0-based index within the whole write.
#[derive(Debug, Clone)]
pub struct RejectedRow {
    pub row_index: u64,
    pub error: String,
}

/// Structured outcome of a batched write.
///
/// `rows_written` + the rejected counts let a host reconcile exactly
/// what landed. `rejected_batches` / `rejected_rows` carry the routable
/// rejects (whole batches by default; individual rows under
/// [`WriteOptions::isolate_failures`]).
#[derive(Debug, Clone, Default)]
pub struct WriteReport {
    /// Total rows the caller handed in.
    pub rows_attempted: u64,
    /// Rows the database accepted.
    pub rows_written: u64,
    /// Batches that committed cleanly.
    pub batches_committed: usize,
    /// Batches rejected as a unit (default granularity).
    pub rejected_batches: Vec<RejectedBatch>,
    /// Rows rejected individually (only populated under
    /// [`WriteOptions::isolate_failures`]).
    pub rejected_rows: Vec<RejectedRow>,
}

impl WriteReport {
    /// True when every attempted row was written.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.rejected_batches.is_empty() && self.rejected_rows.is_empty()
    }
}

/// Write `rows` into `table` on `dst` in bounded back-pressured batches,
/// returning a structured [`WriteReport`].
///
/// `columns` is the destination column order; every row must match it
/// positionally. SQL generation, bulk dispatch, and transaction control
/// are all delegated to the existing copy/transaction machinery (see the
/// [module docs](self)). Rows are pulled from `rows` one batch at a time,
/// so an unbounded iterator is written at `O(batch_size)` memory.
///
/// **Blocking:** issues synchronous statements through `dst`; blocks
/// until the write completes (or the outer transaction rolls back under
/// `atomic`).
///
/// **Atomicity:** with [`WriteOptions::atomic`], all batches run inside
/// one BEGIN/COMMIT (MSSQL `SET XACT_ABORT ON`, Oracle explicit COMMIT);
/// the first failing batch rolls the whole write back and is recorded.
/// Without it, each batch is independent — earlier committed batches
/// stay, later ones still run, and failures are collected.
pub fn write_rows<I>(
    dst: &mut dyn Connection,
    backend: Backend,
    table: &str,
    columns: &[ColumnInfo],
    rows: I,
    opts: &WriteOptions,
) -> Result<WriteReport, SqlError>
where
    I: IntoIterator<Item = Row>,
{
    if opts.mode.needs_key() && opts.key_columns.is_empty() {
        return Err(SqlError::QueryFailed(format!(
            "{:?} write mode requires key_columns (conflict key); none supplied",
            opts.mode
        )));
    }
    // Validate the key columns are part of the destination shape before
    // any row is sent — fail fast, like the copy path's preflight.
    for key in &opts.key_columns {
        if !columns.iter().any(|c| &c.name == key) {
            return Err(SqlError::QueryFailed(format!(
                "key column {key:?} is not among the destination columns"
            )));
        }
    }

    let batch_size = if opts.batch_size == 0 {
        DEFAULT_WRITE_BATCH
    } else {
        opts.batch_size
    };
    let if_exists = opts.mode.if_exists();
    let quoted_table = quote_identifier(table, backend);
    let cols_clause = columns
        .iter()
        .map(|c| quote_identifier(&c.name, backend))
        .collect::<Vec<_>>()
        .join(", ");

    let mut report = WriteReport::default();

    let atomic_opened = if opts.atomic {
        // MSSQL: XACT_ABORT ON makes any statement error abort the whole
        // transaction, matching migrate's atomic-apply contract.
        #[cfg(feature = "mssql")]
        if matches!(backend, Backend::MsSql) {
            let _ = dst.execute("SET XACT_ABORT ON");
        }
        begin_transaction(dst, backend)
    } else {
        false
    };

    let mut iter = rows.into_iter();
    let mut batch: Vec<Row> = Vec::with_capacity(batch_size);
    let mut batch_index = 0usize;
    let mut next_row: u64 = 0;
    let mut atomic_failure: Option<SqlError> = None;

    loop {
        batch.clear();
        for _ in 0..batch_size {
            match iter.next() {
                Some(row) => batch.push(row),
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        let start_row = next_row;
        let n = batch.len();
        report.rows_attempted += n as u64;
        next_row += n as u64;

        match insert_batch(
            dst,
            table,
            columns,
            &opts.key_columns,
            &quoted_table,
            &cols_clause,
            &batch,
            backend,
            if_exists,
            opts.bulk_mode,
            opts.copy_format,
            opts.verbose,
        ) {
            Ok(()) => {
                report.rows_written += n as u64;
                report.batches_committed += 1;
            }
            Err(err) => {
                if atomic_opened {
                    // Under one outer transaction a failed batch dooms
                    // the whole write; stop and roll back below.
                    record_batch_rejection(&mut report, batch_index, start_row, n, &err);
                    atomic_failure = Some(err);
                    break;
                }
                if opts.isolate_failures {
                    // Probe the batch row-by-row to attribute the
                    // failure and let the good rows through.
                    let written = probe_rows(
                        dst,
                        table,
                        columns,
                        &opts.key_columns,
                        &quoted_table,
                        &cols_clause,
                        &batch,
                        backend,
                        if_exists,
                        opts.copy_format,
                        opts.verbose,
                        start_row,
                        &mut report,
                    );
                    report.rows_written += written;
                } else {
                    record_batch_rejection(&mut report, batch_index, start_row, n, &err);
                }
            }
        }
        batch_index += 1;
    }

    if atomic_opened {
        if let Some(err) = atomic_failure {
            let _ = rollback_transaction(dst, backend);
            // The rollback undoes every prior batch, so nothing landed.
            report.rows_written = 0;
            report.batches_committed = 0;
            return Err(SqlError::QueryFailed(format!(
                "atomic write rolled back after batch {} failed: {err}",
                report.rejected_batches.last().map_or(0, |b| b.batch_index)
            )));
        }
        commit_transaction(dst, backend)?;
        // Oracle has no DML autocommit; the explicit COMMIT above already
        // terminated the transaction, so the extra-commit guard is a
        // no-op here but kept for parity with the copy/migrate paths.
        let _ = backend_needs_explicit_commit(backend);
    }

    Ok(report)
}

/// Record a whole-batch rejection in the report.
fn record_batch_rejection(
    report: &mut WriteReport,
    batch_index: usize,
    start_row: u64,
    row_count: usize,
    err: &SqlError,
) {
    report.rejected_batches.push(RejectedBatch {
        batch_index,
        start_row,
        row_count,
        error: err.to_string(),
    });
}

/// Retry a failed batch one row at a time (non-atomic path only),
/// recording each rejected row and returning how many rows were written.
#[allow(clippy::too_many_arguments)]
fn probe_rows(
    dst: &mut dyn Connection,
    table: &str,
    columns: &[ColumnInfo],
    key_columns: &[String],
    quoted_table: &str,
    cols_clause: &str,
    batch: &[Row],
    backend: Backend,
    if_exists: IfExists,
    copy_format: CopyFormat,
    verbose: bool,
    start_row: u64,
    report: &mut WriteReport,
) -> u64 {
    let mut written = 0u64;
    for (offset, row) in batch.iter().enumerate() {
        let single = std::slice::from_ref(row);
        // Force the generic path for the per-row probe: bulk loaders
        // carry no per-row error attribution.
        match insert_batch(
            dst,
            table,
            columns,
            key_columns,
            quoted_table,
            cols_clause,
            single,
            backend,
            if_exists,
            BulkMode::Off,
            copy_format,
            verbose,
        ) {
            Ok(()) => written += 1,
            Err(err) => report.rejected_rows.push(RejectedRow {
                row_index: start_row + offset as u64,
                error: err.to_string(),
            }),
        }
    }
    written
}

// The write-path tests exercise the round trip against an embedded
// SQLite database (always available, no container), so they are gated on
// the `sqlite` feature.
#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::connection::ConnectOptions;
    use crate::url::DatabaseUrl;
    use crate::value::{TypeHint, Value};
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn fresh_sqlite() -> (Box<dyn Connection>, std::path::PathBuf) {
        let pid = std::process::id();
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("ferrule-write-test-{pid}-{n}.db"));
        let _ = std::fs::remove_file(&path);
        let url = DatabaseUrl::parse(&format!("sqlite://{}", path.display())).unwrap();
        let conn = crate::connect(&url, &ConnectOptions::default(), None).unwrap();
        (conn, path)
    }

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            type_hint: TypeHint::Other,
            nullable: true,
        }
    }

    /// Round trip: write N rows in bounded batches and read them back.
    /// `batch_size` smaller than N forces multiple batches, exercising
    /// the bounded back-pressured loop (only one batch buffered).
    #[test]
    fn write_rows_round_trip_in_bounded_batches() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        let columns = vec![col("id"), col("name")];
        let rows: Vec<Row> = (1..=2500)
            .map(|i| vec![Value::Int64(i), Value::String(format!("n{i}"))])
            .collect();
        let opts = WriteOptions {
            batch_size: 100,
            ..Default::default()
        };
        let report = write_rows(&mut *conn, Backend::Sqlite, "t", &columns, rows, &opts).unwrap();
        assert_eq!(report.rows_attempted, 2500);
        assert_eq!(report.rows_written, 2500);
        // ceil(2500 / 100) batches, all committed.
        assert_eq!(report.batches_committed, 25);
        assert!(report.is_complete());

        let back = conn.query("SELECT COUNT(*) FROM t").unwrap();
        assert!(matches!(back.rows[0][0], Value::Int64(2500)));
        let _ = std::fs::remove_file(&path);
    }

    /// Per-batch partial-failure routing: a batch containing a duplicate
    /// PK is rejected as a unit (default granularity), surfaced
    /// structurally, while clean batches still land.
    #[test]
    fn write_rows_rejects_failing_batch_structurally() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        // Seed id=5 so the batch containing it collides.
        conn.execute("INSERT INTO t VALUES (5)").unwrap();
        let columns = vec![col("id")];
        // Two batches of size 4: [1,2,3,4] ok, [5,6,7,8] collides on 5.
        let rows: Vec<Row> = (1..=8).map(|i| vec![Value::Int64(i)]).collect();
        let opts = WriteOptions {
            batch_size: 4,
            ..Default::default()
        };
        let report = write_rows(&mut *conn, Backend::Sqlite, "t", &columns, rows, &opts).unwrap();
        assert_eq!(report.rows_attempted, 8);
        assert_eq!(report.rows_written, 4, "only the clean batch landed");
        assert_eq!(report.batches_committed, 1);
        assert_eq!(report.rejected_batches.len(), 1);
        let rej = &report.rejected_batches[0];
        assert_eq!(rej.batch_index, 1);
        assert_eq!(rej.start_row, 4);
        assert_eq!(rej.row_count, 4);
        assert!(!report.is_complete());
        let _ = std::fs::remove_file(&path);
    }

    /// `isolate_failures` probes the failing batch row-by-row, so the
    /// good rows in a partially-bad batch land and only the true
    /// offender is recorded as a rejected row.
    #[test]
    fn write_rows_isolates_offending_row() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (3)").unwrap();
        let columns = vec![col("id")];
        // One batch [1,2,3,4]; only id=3 collides.
        let rows: Vec<Row> = (1..=4).map(|i| vec![Value::Int64(i)]).collect();
        let opts = WriteOptions {
            batch_size: 10,
            isolate_failures: true,
            ..Default::default()
        };
        let report = write_rows(&mut *conn, Backend::Sqlite, "t", &columns, rows, &opts).unwrap();
        assert_eq!(report.rows_written, 3, "1,2,4 landed; 3 rejected");
        assert_eq!(report.rejected_batches.len(), 0);
        assert_eq!(report.rejected_rows.len(), 1);
        assert_eq!(
            report.rejected_rows[0].row_index, 2,
            "0-based index of id=3"
        );
        let back = conn.query("SELECT COUNT(*) FROM t").unwrap();
        assert!(matches!(back.rows[0][0], Value::Int64(4)));
        let _ = std::fs::remove_file(&path);
    }

    /// `atomic` rolls the entire write back when any batch fails: nothing
    /// from the write survives, even batches that ran before the failure.
    #[test]
    fn write_rows_atomic_rolls_back_on_failure() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (7)").unwrap();
        let columns = vec![col("id")];
        // Batch 0 [1,2] clean, batch 1 [7,8] collides on the seeded 7.
        let rows: Vec<Row> = vec![1, 2, 7, 8]
            .into_iter()
            .map(|i| vec![Value::Int64(i)])
            .collect();
        let opts = WriteOptions {
            batch_size: 2,
            atomic: true,
            ..Default::default()
        };
        let err = write_rows(&mut *conn, Backend::Sqlite, "t", &columns, rows, &opts)
            .expect_err("atomic write must surface the failure");
        assert!(matches!(err, SqlError::QueryFailed(_)));
        // Only the pre-seeded row remains; the write's batch 0 rolled back.
        let back = conn.query("SELECT COUNT(*) FROM t").unwrap();
        assert!(
            matches!(back.rows[0][0], Value::Int64(1)),
            "atomic rollback left only the pre-existing row"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Upsert mode overwrites existing rows by key; reuses the copy
    /// path's ON CONFLICT DO UPDATE builder.
    #[test]
    fn write_rows_upsert_overwrites_by_key() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'old')").unwrap();
        let columns = vec![col("id"), col("v")];
        let rows: Vec<Row> = vec![
            vec![Value::Int64(1), Value::String("new".into())],
            vec![Value::Int64(2), Value::String("two".into())],
        ];
        let opts = WriteOptions {
            mode: WriteMode::Upsert,
            key_columns: vec!["id".into()],
            ..Default::default()
        };
        let report = write_rows(&mut *conn, Backend::Sqlite, "t", &columns, rows, &opts).unwrap();
        assert!(report.is_complete());
        let v1 = conn.query("SELECT v FROM t WHERE id = 1").unwrap();
        assert!(matches!(&v1.rows[0][0], Value::String(s) if s == "new"));
        let _ = std::fs::remove_file(&path);
    }

    /// Skip/Upsert without a key is rejected before any row is sent.
    #[test]
    fn write_rows_conflict_mode_requires_key() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let columns = vec![col("id")];
        let opts = WriteOptions {
            mode: WriteMode::Skip,
            ..Default::default()
        };
        let err = write_rows(
            &mut *conn,
            Backend::Sqlite,
            "t",
            &columns,
            vec![vec![Value::Int64(1)]],
            &opts,
        )
        .expect_err("skip without key must fail fast");
        assert!(matches!(err, SqlError::QueryFailed(_)));
        let _ = std::fs::remove_file(&path);
    }

    /// A key column not present in the destination shape fails fast.
    #[test]
    fn write_rows_unknown_key_column_fails_fast() {
        let (mut conn, path) = fresh_sqlite();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let columns = vec![col("id")];
        let opts = WriteOptions {
            mode: WriteMode::Upsert,
            key_columns: vec!["nonexistent".into()],
            ..Default::default()
        };
        let err = write_rows(
            &mut *conn,
            Backend::Sqlite,
            "t",
            &columns,
            vec![vec![Value::Int64(1)]],
            &opts,
        )
        .expect_err("unknown key column must fail fast");
        assert!(matches!(err, SqlError::QueryFailed(_)));
        let _ = std::fs::remove_file(&path);
    }
}
