use thiserror::Error;

/// Errors originating in the `ferrule-sql` driver and write-path layer.
///
/// Every backend method, URL parse, connect dispatch, transaction
/// helper, and copy routine returns this type. Variant names and tuple
/// shapes are load-bearing across the workspace (the CLI pattern-matches
/// [`SqlError::QueryFailed`] in several hot paths), so preserve them when
/// editing.
///
/// `RegistryError` is registry/CLI-level rather than driver-level; it
/// rides along here as a deliberate minimal-diff choice during the
/// `ferrule-core` -> `ferrule-sql` extraction and is a candidate for a
/// later relocation to a core-side error type.
#[derive(Error, Debug)]
pub enum SqlError {
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

    /// Backend-native bulk-load path is not usable at runtime (server
    /// config, missing capability, permission denied, target relation
    /// is not a base table, etc.). Callers may retry on the generic
    /// INSERT path. The string is intended for stderr only; do not
    /// match on it.
    #[error("bulk path unavailable: {0}")]
    BulkUnavailable(String),

    #[error("TLS error: {0}")]
    TlsError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Host key mismatch during SSH tunnel setup (always fatal).
    #[cfg(feature = "ssh")]
    #[error("SSH host key mismatch for {host}:{port}")]
    SshHostKeyMismatch { host: String, port: u16 },

    /// Unknown host during SSH tunnel setup (can be TOFU-prompted
    /// interactively by the CLI layer).
    #[cfg(feature = "ssh")]
    #[error("SSH unknown host {host}:{port}")]
    SshUnknownHost {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
        key: russh::keys::ssh_key::PublicKey,
    },

    #[error("timeout")]
    Timeout,

    #[error("registry error: {0}")]
    RegistryError(String),
}
