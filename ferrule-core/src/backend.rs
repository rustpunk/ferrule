#![allow(dead_code, unused_variables, unused_imports)]

use crate::backends;
use crate::connection::{Connection, ConnectOptions};
use crate::error::CoreError;
use crate::url::DatabaseUrl;

/// Supported database backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    #[cfg(feature = "postgres")]
    Postgres,
    #[cfg(feature = "mysql")]
    MySql,
    #[cfg(feature = "mssql")]
    MsSql,
    #[cfg(feature = "sqlite")]
    Sqlite,
    #[cfg(feature = "oracle")]
    Oracle,
}

impl Backend {
    /// Resolve a backend from a URL scheme.
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            #[cfg(feature = "postgres")]
            "postgres" | "postgresql" => Some(Self::Postgres),
            #[cfg(feature = "mysql")]
            "mysql" | "mariadb" => Some(Self::MySql),
            #[cfg(feature = "mssql")]
            "mssql" | "sqlserver" | "tds" => Some(Self::MsSql),
            #[cfg(feature = "sqlite")]
            "sqlite" => Some(Self::Sqlite),
            #[cfg(feature = "oracle")]
            "oracle" => Some(Self::Oracle),
            _ => None,
        }
    }

    /// Human-readable name.
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres => "PostgreSQL",
            #[cfg(feature = "mysql")]
            Self::MySql => "MySQL",
            #[cfg(feature = "mssql")]
            Self::MsSql => "Microsoft SQL Server",
            #[cfg(feature = "sqlite")]
            Self::Sqlite => "SQLite",
            #[cfg(feature = "oracle")]
            Self::Oracle => "Oracle",
        }
    }
}

/// Establish a connection to the given URL.
pub async fn connect(
    url: &DatabaseUrl,
    opts: &ConnectOptions,
) -> Result<Box<dyn Connection>, CoreError> {
    let backend = Backend::from_scheme(url.scheme())
        .ok_or_else(|| CoreError::UnsupportedScheme(url.scheme().to_string()))?;

    match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let conn = backends::postgres::connect(url, opts).await?;
            Ok(Box::new(conn))
        }
        #[cfg(feature = "mysql")]
        Backend::MySql => {
            let conn = backends::mysql::connect(url, opts).await?;
            Ok(Box::new(conn))
        }
        #[cfg(feature = "mssql")]
        Backend::MsSql => {
            let conn = backends::mssql::connect(url, opts).await?;
            Ok(Box::new(conn))
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => {
            let conn = backends::sqlite::connect(url, opts).await?;
            Ok(Box::new(conn))
        }
        #[cfg(feature = "oracle")]
        Backend::Oracle => {
            let conn = backends::oracle::connect(url, opts).await?;
            Ok(Box::new(conn))
        }
    }
}
