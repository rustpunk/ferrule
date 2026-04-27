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
}
