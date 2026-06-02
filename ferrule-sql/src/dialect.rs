//! SQL dialect classification shared across the driver and write-path
//! modules.
//!
//! [`Dialect`] is a pure data classification of the SQL flavour a
//! connection speaks, derived from the URL scheme. It carries no driver
//! dependency and is deliberately not feature-gated so that callers can
//! reason about portable DDL/query generation regardless of which
//! backend features are compiled in.

/// SQL dialect of the target database, used to generate portable DDL
/// and queries.
///
/// Derived from the connection URL scheme (see [`Dialect::from_scheme`]).
/// Deliberately not feature-gated: it is a pure data classification of
/// the SQL we emit and carries no driver dependency, so it stays
/// available regardless of which backend features are compiled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    MySql,
    MsSql,
    Oracle,
}

impl Dialect {
    /// Map a connection URL scheme to a [`Dialect`].
    ///
    /// Returns `None` for unrecognised schemes; callers fall back to
    /// [`Dialect::Sqlite`] semantics (ANSI `LIMIT`, `TEXT` columns).
    #[must_use]
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            "sqlite" => Some(Self::Sqlite),
            "postgres" | "postgresql" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::MySql),
            "mssql" | "sqlserver" | "tds" => Some(Self::MsSql),
            "oracle" => Some(Self::Oracle),
            _ => None,
        }
    }
}
