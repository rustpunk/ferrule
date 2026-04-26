//! `ferrule-core` — Backend drivers, unified types, URL parsing, and formatters.

pub mod backend;
pub mod connection;
pub mod error;
pub mod formatter;
pub mod url;
pub mod value;

mod backends;

pub use backend::{Backend, connect};
pub use connection::{Connection, ExecutionSummary, QueryResult, ConnectOptions, StatementResult};
pub use error::CoreError;
pub use formatter::{OutputFormat, format_result};
pub use url::DatabaseUrl;
pub use value::{ColumnInfo, Row, TypeHint, Value};
