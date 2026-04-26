use thiserror::Error;

/// Errors originating in `ferrule-config`.
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    ConfigNotFound(String),

    #[error("invalid config: {0}")]
    InvalidConfig(String),

    #[error("connection '{0}' not found in registry")]
    ConnectionNotFound(String),

    #[error("duplicate connection name: {0}")]
    DuplicateConnection(String),

    #[error("profile '{0}' not found")]
    ProfileNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("keyring error: {0}")]
    KeyringError(String),
}
