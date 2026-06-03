//! `ferrule-sql` — the embeddable SQL driver and write-path core.
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
pub use backend::{connect, Backend};
pub use connection::{
    BulkInsert, ConnectOptions, Connection, ExecutionSummary, ForeignKey, QueryResult,
    StatementResult,
};
pub use copy::{
    copy_all_tables, copy_rows, discover_tables, quote_identifier, topo_sort, translate_ddl,
    translate_type, AllTablesOptions, BulkMode, CopyFormat, CopyOptions, CopySource, CycleError,
    IfExists,
};
pub use dialect::Dialect;
pub use error::SqlError;
pub use guard::SizeGuards;
pub use proxy::{is_no_proxy, resolve_proxy_from_env, ProxiedConnection, ProxyConfig};
pub use query_builder::apply_paging;
pub use render::{quote_string, render_value};
pub use stream::{BoxRowStream, RowCursor, DEFAULT_CURSOR_CAPACITY};
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
    write_rows, BatchOutcome, RejectedBatch, RejectedRow, WriteMode, WriteOptions, WriteReport,
    DEFAULT_WRITE_BATCH,
};
