//! `ferrule-core` — Backend drivers, unified types, URL parsing, and formatters.

pub mod backend;
pub mod connection;
pub mod dump;
pub mod error;
pub mod explain;
pub mod formatter;
pub mod load;
pub mod params;
pub mod url;
pub mod value;

pub mod query_builder;

mod backends;

pub use backend::{connect, Backend};
pub use connection::{ConnectOptions, Connection, ExecutionSummary, QueryResult, StatementResult};
pub use dump::{dump_query, dump_table, DumpFormat, DumpOptions};
pub use error::CoreError;
pub use explain::{explain_sql, is_modifying, ExplainOutput};
pub use formatter::{format_result, OutputFormat};
pub use load::{infer_schema, load_data, LoadFormat, LoadOptions};
pub use params::{infer_type, load_from_json, parse_param, quote_string, substitute, ParameterSet};
pub use query_builder::apply_paging;
pub use url::DatabaseUrl;
pub use value::{ColumnInfo, Row, TypeHint, Value};
