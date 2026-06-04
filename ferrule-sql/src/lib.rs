//! `ferrule-sql` — the embeddable, synchronous, bounded-memory SQL core.
//!
//! This crate owns the unified neutral [`Value`]/[`Row`] types, the
//! [`DatabaseUrl`] parser, the [`Connection`] trait and its per-backend
//! drivers, the connect dispatcher (direct, HTTP-proxy, and SSH-tunnel
//! transports), the transaction helpers, and the cross-backend copy /
//! bulk-load write path. It carries no rendering (`tabled`) or
//! credential-resolution (`ferrule-config`) dependency, so it can be
//! embedded by callers that supply already-resolved connection details.
//!
//! Backends are feature-gated (`postgres`, `mysql`, `mssql`, `sqlite`,
//! `oracle`); the SSH tunnel transport is behind `ssh`. The `default`
//! feature set is empty — enable the backends you need.
//!
//! # Why these properties
//!
//! - **Synchronous public API.** No `async fn` / `Future` crosses the
//!   crate boundary, so a host with no runtime of its own can call
//!   straight through. The async network drivers are still used — each
//!   [`Connection`] owns a private current-thread `tokio` runtime and
//!   blocks on it — but that runtime is an implementation detail. (SQLite
//!   and Oracle are natively synchronous and call through directly.)
//! - **Bounded memory.** [`Connection::query_cursor`] streams from a
//!   native database cursor one back-pressured batch at a time, and
//!   [`write_rows`] flushes an arbitrarily large row iterator in
//!   fixed-size batches — both stay `O(batch)` regardless of total row
//!   count. The eager [`Connection::query`] still materializes a full
//!   `Vec<Row>`, but it is capped by per-cell / per-row / per-result
//!   [`SizeGuards`] so a pathological result fails fast instead of OOMing.
//! - **Caller-resolved credentials.** [`connect`] takes the password as a
//!   [`secrecy::SecretString`] on [`ConnectOptions`]; the crate performs
//!   no credential resolution and depends on no keyring / prompt library.
//!
//! # Embedding flow
//!
//! The three steps a host follows — **connect → streaming read → batched
//! write** — line up with the runnable
//! [`examples/embed.rs`](https://github.com/rustpunk/ferrule/blob/main/ferrule-sql/examples/embed.rs)
//! (`cargo run -p ferrule-sql --example embed --features sqlite`):
//!
//! ### 1. Connect with a resolved secret
//!
//! Parse a [`DatabaseUrl`], hand the host-resolved credential to
//! [`ConnectOptions::password`], and call [`connect`]. The returned
//! [`Connection`] owns its private runtime and blocks on every call.
//!
//! ### 2. Streaming read (the bounded cursor)
//!
//! [`Connection::query_cursor`] returns a [`RowCursor`]. Pull rows with
//! [`RowCursor::next_batch(n)`](RowCursor::next_batch) (a bounded chunk)
//! or by iterating; either way the driver only fetches more from the
//! server as you consume, so peak memory is `O(batch)`, never the whole
//! result. The cursor borrows the connection for its lifetime, so scope
//! it before issuing the next statement.
//!
//! ### 3. Batched write (back-pressured, structured report)
//!
//! [`write_rows`] consumes any `IntoIterator<Item = Row>` and flushes it
//! in fixed-size batches ([`WriteOptions::batch_size`]), buffering one
//! batch at a time. It reuses the cross-DB copy path's SQL generation and
//! transaction control and returns a [`WriteReport`] naming exactly which
//! batches / rows landed and which were rejected. Pair it with the
//! cursor from step 2 for an end-to-end bounded-memory pipe.
//!
//! ```no_run
//! # #[cfg(feature = "sqlite")]
//! # fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use ferrule_sql::{
//!     connect, write_rows, Backend, ColumnInfo, ConnectOptions, DatabaseUrl,
//!     Row, TypeHint, Value, WriteOptions,
//! };
//! use secrecy::SecretString;
//!
//! // 1. Connect. The password is a caller-resolved `SecretString`;
//! //    SQLite ignores it, a networked backend would consume it.
//! let url = DatabaseUrl::parse("sqlite:///tmp/embed-demo.db")?;
//! let opts = ConnectOptions {
//!     insecure: false,
//!     password: Some(SecretString::from("resolved-by-the-host")),
//! };
//! let mut conn = connect(&url, &opts, None)?;
//! conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")?;
//!
//! // 2. Streaming read at bounded memory: two rows per pull.
//! {
//!     let mut cursor = conn.query_cursor("SELECT id, name FROM t ORDER BY id")?;
//!     while !cursor.next_batch(2)?.is_empty() {
//!         // process this bounded batch, then pull the next one
//!     }
//! }
//!
//! // 3. Batched write: one batch buffered at a time.
//! let columns = [
//!     ColumnInfo { name: "id".into(), type_hint: TypeHint::Int64, nullable: false },
//!     ColumnInfo { name: "name".into(), type_hint: TypeHint::String, nullable: true },
//! ];
//! let rows: Vec<Row> = (1..=1000)
//!     .map(|i| vec![Value::Int64(i), Value::String(format!("n{i}"))])
//!     .collect();
//! let report = write_rows(
//!     &mut *conn,
//!     Backend::Sqlite,
//!     "t",
//!     &columns,
//!     rows,
//!     &WriteOptions { batch_size: 200, ..Default::default() },
//! )?;
//! assert!(report.is_complete());
//! # Ok(())
//! # }
//! ```
//!
//! # Reentrancy
//!
//! The private runtime is current-thread, so a [`Connection`] (and its
//! [`RowCursor`]) must not be driven from inside another `block_on` on the
//! same thread. An async host hops to a blocking thread
//! (`tokio::task::spawn_blocking` or a dedicated OS thread) first.

#![allow(dead_code, unused_variables, unused_imports)]

pub mod backend;
pub mod connection;
pub mod copy;
pub mod dialect;
pub mod error;
pub mod guard;
pub mod proxy;
pub mod query_builder;
pub mod render;
pub mod stream;
pub mod sync;
pub mod transaction;
pub mod tunnel;
pub mod url;
pub mod value;
pub mod write;

/// Per-backend driver modules, one feature-gated submodule per backend.
///
/// The module is `pub` so the per-backend concrete connection types and
/// their inline integration tests are reachable, but the connection
/// *constructors* are `pub(crate)`: every caller establishes connections
/// through the synchronous URL-scheme dispatcher [`connect`], which is
/// the only blocking entry point and the one that owns the private
/// runtime. Embedders never touch a driver's async constructor directly.
pub mod backends;

#[cfg(feature = "ssh")]
pub use backend::connect_with_tunnel;
pub use backend::{Backend, connect};
pub use connection::{
    BulkInsert, ConnectOptions, Connection, ExecutionSummary, ForeignKey, QueryResult, SchemaInfo,
    StatementResult,
};
pub use copy::{
    AllTablesOptions, BulkMode, CopyFormat, CopyOptions, CopySource, CycleError, IfExists,
    copy_all_tables, copy_rows, discover_tables, quote_identifier, topo_sort, translate_ddl,
    translate_type,
};
pub use dialect::Dialect;
pub use error::SqlError;
pub use guard::SizeGuards;
pub use proxy::{ProxiedConnection, ProxyConfig, is_no_proxy, resolve_proxy_from_env};
pub use query_builder::apply_paging;
pub use render::{quote_string, render_value};
pub use stream::{BoxRowStream, DEFAULT_CURSOR_CAPACITY, RowCursor};
pub use sync::SyncConnection;
pub use transaction::{begin_transaction, commit_transaction, rollback_transaction};
pub use tunnel::SshConfig;
#[cfg(feature = "ssh")]
pub use tunnel::{
    KeySource, SshSession, TunnelError, TunnelHandle, TunnelStream, TunnelTransport,
    TunnelTransportResult, TunneledConnection,
};
pub use url::DatabaseUrl;
pub use value::{ColumnInfo, Row, TypeHint, Value};
pub use write::{
    BatchOutcome, DEFAULT_WRITE_BATCH, RejectedBatch, RejectedRow, WriteMode, WriteOptions,
    WriteReport, write_rows,
};
