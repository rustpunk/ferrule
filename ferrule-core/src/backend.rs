use crate::backends;
use crate::connection::{ConnectOptions, Connection};
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
        match *self {
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

/// Establish a connection to `url` through an SSH tunnel.
///
/// Picks the transport based on the backend:
///
/// - **Postgres** — `Stream` transport. The russh channel is fed
///   directly into `tokio_postgres::Config::connect_raw`, so there
///   is no extra TCP hop. TLS (if requested via `?sslmode=...`) is
///   negotiated end-to-end inside the SSH stream.
/// - **MySQL, MSSQL, Oracle** — `LocalListener` transport. A
///   `127.0.0.1:<random>` listener is bound; bytes are pumped through
///   the SSH channel by a forwarder task. The driver opens a regular
///   TCP connection to that local port.
/// - **SQLite** — rejected. SQLite is a local-file backend, so SSH
///   tunneling is not applicable.
///
/// The returned `Box<dyn Connection>` wraps the inner backend
/// connection in a [`crate::tunnel::TunneledConnection`] that owns
/// the SSH session (and, for the LocalListener path, the forwarder
/// task) for the connection's lifetime.
#[cfg(feature = "ssh")]
pub async fn connect_with_tunnel(
    url: &DatabaseUrl,
    opts: &ConnectOptions,
    ssh_config: &crate::tunnel::SshConfig,
    key_source: &crate::tunnel::KeySource,
) -> Result<Box<dyn Connection>, CoreError> {
    use crate::tunnel::{
        setup_tunnel, TunnelError, TunnelTransport, TunnelTransportResult, TunneledConnection,
    };

    let backend = Backend::from_scheme(url.scheme())
        .ok_or_else(|| CoreError::UnsupportedScheme(url.scheme().to_string()))?;

    #[cfg(feature = "sqlite")]
    if matches!(backend, Backend::Sqlite) {
        return Err(CoreError::ConnectionFailed(
            "SSH tunneling is not applicable to SQLite (local-file backend)".to_string(),
        ));
    }

    let target_host = url
        .host()
        .ok_or_else(|| {
            CoreError::ConnectionFailed(
                "URL has no host — SSH tunneling requires a network-based backend"
                    .to_string(),
            )
        })?
        .to_string();
    let target_port = url
        .port()
        .unwrap_or_else(|| default_port_for(backend));

    let transport = match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => TunnelTransport::Stream,
        _ => TunnelTransport::LocalListener,
    };

    let tunnel = setup_tunnel(
        ssh_config,
        key_source,
        &target_host,
        target_port,
        transport,
    )
    .await
    .map_err(|e| match e {
        TunnelError::HostKeyMismatch { host, port, .. } => {
            CoreError::SshHostKeyMismatch { host, port }
        }
        TunnelError::UnknownHost { host, port, algorithm, fingerprint, key, .. } => {
            CoreError::SshUnknownHost { host, port, algorithm, fingerprint, key }
        }
        other => CoreError::ConnectionFailed(format!("SSH tunnel setup: {other}")),
    })?;

    let session = tunnel.session;
    match tunnel.transport {
        TunnelTransportResult::Stream { stream } => {
            #[cfg(feature = "postgres")]
            if matches!(backend, Backend::Postgres) {
                let pg = backends::postgres::connect_with_stream(url, opts, *stream).await?;
                return Ok(Box::new(TunneledConnection {
                    inner: Box::new(pg),
                    session,
                    forwarder: None,
                }));
            }
            // The transport-selection match above only picks Stream
            // for Postgres, so this is unreachable in practice. The
            // explicit error keeps the code honest if a future
            // refactor adds another `Stream`-eligible backend without
            // wiring the receive side here.
            Err(CoreError::ConnectionFailed(
                "Stream transport selected but no backend handler is registered \
                 (this is a ferrule bug — please report)"
                    .to_string(),
            ))
        }
        TunnelTransportResult::LocalPort { port, forwarder } => {
            let local_url = rewrite_url_to_local(url, port)?;
            let inner = connect(&local_url, opts).await?;
            Ok(Box::new(TunneledConnection {
                inner,
                session,
                forwarder: Some(forwarder),
            }))
        }
    }
}

#[cfg(feature = "ssh")]
fn default_port_for(backend: Backend) -> u16 {
    match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => 5432,
        #[cfg(feature = "mysql")]
        Backend::MySql => 3306,
        #[cfg(feature = "mssql")]
        Backend::MsSql => 1433,
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => 0, // unreachable — sqlite is rejected upstream
        #[cfg(feature = "oracle")]
        Backend::Oracle => 1521,
    }
}

#[cfg(feature = "ssh")]
fn rewrite_url_to_local(url: &DatabaseUrl, port: u16) -> Result<DatabaseUrl, CoreError> {
    let mut parsed = ::url::Url::parse(url.raw())
        .map_err(|e| CoreError::InvalidUrl(format!("re-parse for tunnel rewrite: {e}")))?;
    parsed
        .set_host(Some("127.0.0.1"))
        .map_err(|e| CoreError::InvalidUrl(format!("set_host(127.0.0.1): {e}")))?;
    parsed
        .set_port(Some(port))
        .map_err(|()| CoreError::InvalidUrl("set_port failed".to_string()))?;
    DatabaseUrl::parse(parsed.as_str())
}
