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

/// A blocking [`Connection`] backed by an async driver and a private
/// current-thread runtime.
///
/// **Blocking model.** Every method calls `self.rt.block_on(...)` on the
/// owned runtime, so it blocks the calling thread until the driver
/// future resolves. **Memory model.** Results are fully buffered (see
/// [`Connection`]). **Reentrancy.** The runtime is current-thread; do
/// not call from inside another `block_on` on the same thread (hop to a
/// blocking thread first).
pub struct SyncConnection {
    /// The wrapped async connection. Declared **before** `rt` so that
    /// Rust's declaration-order field drop tears this connection down —
    /// together with any background I/O task it spawned on the runtime —
    /// while the runtime is still alive, and only then drops `rt`. A
    /// driver whose own `Drop` touches the runtime therefore stays sound;
    /// today none do, so the ordering is defensive but deliberate.
    inner: Box<dyn AsyncConnection>,
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
        Self { rt, inner }
    }
}

impl Connection for SyncConnection {
    fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.execute(sql))
    }

    fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError> {
        let inner = &mut self.inner;
        self.rt.block_on(inner.query(sql))
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
