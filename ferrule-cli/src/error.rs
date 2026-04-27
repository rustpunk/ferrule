use std::fmt;
use thiserror::Error;

/// Exit codes used by the ferrule binary.
///
/// `0` (success) is implicit via Rust's normal `main()` return.
///
/// Code `1` is reserved for "command succeeded with a notable result"
/// — the GNU `diff` / `grep` / `kubectl diff` convention. It covers
/// `ferrule diff` finding schema differences today, and is the slot
/// for a future `--fail-on-empty` flag and any future check / validate
/// / lint commands. The semantic is "the command ran correctly and
/// the result is something the caller likely wants to gate on" — not
/// an error.
///
/// Codes `2..=N` are real errors. `2` matches clap's default exit for
/// argument-parse failures, so usage errors raised by ferrule itself
/// and usage errors raised by clap end up at the same code.
pub mod exit {
    pub const RESULT_NOTABLE: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const CONNECTION: i32 = 3;
    pub const QUERY: i32 = 4;
}

/// CLI-level error type.
///
/// Every variant carries a semantic category that maps to a stable exit code.
/// Library errors (`CoreError`, `ConfigError`) are *not* given blanket `From`
/// impls because the same `CoreError` can mean "connection" in one command and
/// "query" in another.  Each call site must explicitly choose its category via
/// the constructors below so that exit-code classification is never implicit.
#[derive(Debug, Error)]
pub enum CliError {
    #[error("connection failed: {0}")]
    Connection(ferrule_core::CoreError),

    #[error("query failed: {0}")]
    Query(ferrule_core::CoreError),

    #[error("registry error: {0}")]
    Registry(ferrule_config::error::ConfigError),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid usage: {0}")]
    Usage(String),
}

impl CliError {
    pub fn connection(e: ferrule_core::CoreError) -> Self {
        Self::Connection(e)
    }

    pub fn query(e: ferrule_core::CoreError) -> Self {
        Self::Query(e)
    }

    pub fn registry(e: ferrule_config::error::ConfigError) -> Self {
        Self::Registry(e)
    }

    pub fn usage<S: Into<String>>(msg: S) -> Self {
        Self::Usage(msg.into())
    }

    /// Process exit code dictated by the error category.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Connection(_) => exit::CONNECTION,
            Self::Query(_) => exit::QUERY,
            Self::Registry(_) => exit::CONNECTION,
            Self::Io(_) => exit::QUERY,
            Self::Usage(_) => exit::USAGE,
        }
    }
}

impl miette::Diagnostic for CliError {
    fn code<'a>(&'a self) -> Option<Box<dyn fmt::Display + 'a>> {
        Some(Box::new(match self {
            Self::Connection(_) => "ferrule::connection",
            Self::Query(_) => "ferrule::query",
            Self::Registry(_) => "ferrule::registry",
            Self::Io(_) => "ferrule::io",
            Self::Usage(_) => "ferrule::usage",
        }))
    }
}
