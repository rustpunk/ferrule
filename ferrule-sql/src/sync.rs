//! Synchronous wrapper that turns an async backend driver into the
//! public blocking [`Connection`] API.
//!
//! `ferrule-sql`'s drivers (`tokio-postgres`, `mysql_async`, `tiberius`)
//! are async. Rather than expose that async surface to embedders — many
//! of which run no runtime of their own — every connection handle owns
//! one **private current-thread `tokio` runtime** and drives each driver
//! future to completion with `block_on`. The runtime is created once at
//! connect time and lives inside [`SyncConnection`] for the connection's
//! lifetime, so the same runtime that spawned a driver's background I/O
//! task (e.g. tokio-postgres' connection task) also polls it on every
//! subsequent call. No `async fn` / `Future` crosses the public boundary.

use crate::connection::{
    AsyncConnection, BulkInsert, Connection, ExecutionSummary, ForeignKey, QueryResult,
    StatementResult,
};
use crate::error::SqlError;
use crate::guard::SizeGuards;
use crate::stream::RowCursor;

/// A blocking [`Connection`] backed by an async driver and a private
/// current-thread runtime.
///
/// **Blocking model.** Every method calls `self.rt.block_on(...)` on the
/// owned runtime, so it blocks the calling thread until the driver
/// future resolves. **Memory model.** [`query`](Connection::query)
/// buffers the result but is bounded by
/// [`size_guards`](Connection::size_guards) — an oversized cell/row or a
/// result past the total cap fails fast; [`query_cursor`](Connection::query_cursor)
/// streams at bounded memory. **Reentrancy.** The runtime is
/// current-thread; do not call from inside another `block_on` on the
/// same thread (hop to a blocking thread first).
pub struct SyncConnection {
    /// The wrapped async connection. Declared **before** `rt` so that
    /// Rust's declaration-order field drop tears this connection down —
    /// together with any background I/O task it spawned on the runtime —
    /// while the runtime is still alive, and only then drops `rt`. A
    /// driver whose own `Drop` touches the runtime therefore stays sound;
    /// today none do, so the ordering is defensive but deliberate.
    inner: Box<dyn AsyncConnection>,
    /// Per-cell / per-row / per-result byte ceilings applied to every
    /// read (both the eager `query` and the streaming `query_cursor`).
    /// Defaults to [`SizeGuards::default`]; override with
    /// [`set_size_guards`](Connection::set_size_guards).
    guards: SizeGuards,
    /// The private current-thread `tokio` runtime that drives every
    /// driver future via `block_on`. Declared **after** `inner` so it is
    /// dropped last, outliving the connection it powers.
    rt: tokio::runtime::Runtime,
}

impl SyncConnection {
    /// Wrap an async connection plus the runtime that must drive it.
    ///
    /// The runtime passed here MUST be the same one used to establish
    /// `inner` (and to spawn any driver background task), so that those
    /// tasks keep being polled on later `block_on` calls.
    #[must_use]
    pub(crate) fn new(rt: tokio::runtime::Runtime, inner: Box<dyn AsyncConnection>) -> Self {
        Self {
            rt,
            inner,
            guards: SizeGuards::default(),
        }
    }
}

impl Connection for SyncConnection {
    fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.execute(sql))
    }

    /// Eager read, routed through the **native cursor** and collected
    /// into a fully-materialized [`QueryResult`]. Building the eager
    /// result on top of the streaming producer keeps a single decode
    /// path shared with [`query_cursor`](Self::query_cursor); for the
    /// network backends it also means the rows are pulled from the
    /// server's cursor rather than pre-buffered by the driver.
    fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError> {
        let inner = &mut self.inner;
        let guards = self.guards;
        self.rt.block_on(async move {
            use futures_util::stream::StreamExt;
            let (columns, mut stream) = inner.query_stream(sql).await?;
            let mut rows = Vec::new();
            let mut total: usize = 0;
            let mut ordinal: u64 = 0;
            while let Some(item) = stream.next().await {
                let row = item?;
                // Per-cell / per-row caps: fail fast before retaining the
                // row. Total-buffer cap: bound the eager result so the
                // CLI table path cannot collect an unbounded `Vec<Row>`.
                guards.check_row(ordinal, &row, &columns)?;
                if guards.caps_total() {
                    let row_bytes: usize = row.iter().map(crate::value::Value::byte_size).sum();
                    total = total.saturating_add(row_bytes);
                    if total > guards.max_total_buffered_bytes {
                        return Err(SqlError::BufferTooLarge {
                            rows_buffered: ordinal,
                            cap: guards.max_total_buffered_bytes,
                        });
                    }
                }
                ordinal += 1;
                rows.push(row);
            }
            Ok(QueryResult { columns, rows })
        })
    }

    fn query_cursor(&mut self, sql: &str) -> Result<RowCursor<'_>, SqlError> {
        // Split-borrow the disjoint fields: the stream borrows `inner`,
        // the cursor drives it through `rt`. No self-referential storage
        // and no `unsafe` — `RowCursor` holds both borrows for its
        // lifetime, which is why it exclusively borrows the connection.
        let rt = &self.rt;
        let inner = &mut self.inner;
        let guards = self.guards;
        let (columns, stream) = rt.block_on(inner.query_stream(sql))?;
        Ok(RowCursor::new(columns, rt, stream, guards))
    }

    fn size_guards(&self) -> SizeGuards {
        self.guards
    }

    fn set_size_guards(&mut self, guards: SizeGuards) {
        self.guards = guards;
    }

    fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.execute_multi(sql))
    }

    fn ping(&mut self) -> Result<(), SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.ping())
    }

    fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.list_tables(schema))
    }

    fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.describe_table(schema, table))
    }

    fn primary_key(&mut self, schema: Option<&str>, table: &str) -> Result<Vec<String>, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.primary_key(schema, table))
    }

    fn list_foreign_keys(&mut self, schema: Option<&str>) -> Result<Vec<ForeignKey>, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.list_foreign_keys(schema))
    }

    fn bulk_insert_rows(&mut self, target: BulkInsert<'_>) -> Result<usize, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.bulk_insert_rows(target))
    }
}
