//! `ferrule-core` — the CLI-support layer over the `ferrule-sql` driver core.
//!
//! The neutral `Value`/`Row` types, the `DatabaseUrl` parser, the
//! `Connection` trait and its per-backend drivers, the connect
//! dispatcher, transaction helpers, the cross-backend copy / bulk-load
//! write path, the `Dialect` classifier, and the `SqlError` error type
//! all live in [`ferrule_sql`]; import them from there. This crate keeps
//! the layers that sit above the driver and pull in dependencies
//! (`tabled`, `csv`, `ferrule-config`) that the embeddable SQL core
//! deliberately avoids: result formatting, dump/load, the migration
//! engine, EXPLAIN wrapping, connection resolution, and parameter
//! substitution.

pub mod dump;
pub mod explain;
pub mod formatter;
pub mod load;
pub mod migrate;
pub mod params;
pub mod resolver;

pub use dump::{dump_query, dump_table, DumpFormat, DumpOptions};
pub use explain::{explain_sql, is_modifying, ExplainOutput};
pub use formatter::{format_result, OutputFormat};
pub use load::{infer_schema, load_data, LoadFormat, LoadOptions};
pub use params::{infer_type, load_from_json, parse_param, substitute, ParameterSet};
