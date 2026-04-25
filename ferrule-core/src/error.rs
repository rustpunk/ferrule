#![allow(dead_code, unused_variables, unused_imports)]

use thiserror::Error;

/// Errors originating in `ferrule-core`.
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    #[error("invalid connection URL: {0}")]
    InvalidUrl(String),

    #[error("backend '{0}' is not enabled — recompile with the appropriate feature")]
    BackendDisabled(String),

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("query failed: {0}")]
    QueryFailed(String),

    #[error("TLS error: {0}")]
    TlsError(String),

    #[error("timeout")]
    Timeout,
}
