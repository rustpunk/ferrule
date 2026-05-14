//! `ferrule-core` — Backend drivers, unified types, URL parsing, and formatters.

pub mod backend;
pub mod connection;
pub mod copy;
pub mod dump;
pub mod error;
pub mod explain;
pub mod formatter;
pub mod load;
pub mod migrate;
pub mod params;
pub mod proxy;
pub mod resolver;
pub mod tunnel;
pub mod url;
pub mod value;

pub mod query_builder;

mod backends;

#[cfg(feature = "ssh")]
pub use backend::connect_with_tunnel;
pub use backend::{connect, Backend};
pub use connection::{
    BulkInsert, ConnectOptions, Connection, ExecutionSummary, ForeignKey, QueryResult,
    StatementResult,
};
pub use copy::{
    copy_all_tables, copy_rows, discover_tables, topo_sort, translate_ddl, translate_type,
    AllTablesOptions, BulkMode, CopyFormat, CopyOptions, CopySource, CycleError, IfExists,
};
pub use dump::{dump_query, dump_table, DumpFormat, DumpOptions};
pub use error::CoreError;
pub use explain::{explain_sql, is_modifying, ExplainOutput};
pub use formatter::{format_result, OutputFormat};
pub use load::{infer_schema, load_data, LoadFormat, LoadOptions};
pub use params::{infer_type, load_from_json, parse_param, quote_string, substitute, ParameterSet};
pub use proxy::{
    http_connect, is_no_proxy, resolve_proxy_from_env, ProxiedConnection, ProxyConfig,
};
pub use query_builder::apply_paging;
pub use tunnel::SshConfig;
#[cfg(feature = "ssh")]
pub use tunnel::{
    setup_tunnel, KeySource, SshSession, TunnelError, TunnelHandle, TunnelStream, TunnelTransport,
    TunnelTransportResult, TunneledConnection,
};
pub use url::DatabaseUrl;
pub use value::{ColumnInfo, Row, TypeHint, Value};
