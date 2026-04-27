//! SSH tunnel support — types and lifecycle.
//!
//! [`SshConfig`] is the validated output of merging profile keys and
//! CLI flags. It is the type backends consume to set up a tunnel
//! before opening their underlying connection.
//!
//! The russh-backed transport (session, channel, port forwarding,
//! [`TunneledConnection`] wrapper) lives behind the `ssh` Cargo
//! feature. The hybrid transport architecture is documented inline at
//! [`TunnelTransport`]:
//!
//! - **`LocalListener`** — binds `127.0.0.1:0`, pumps bytes through
//!   an SSH direct-tcpip channel. Used by every backend whose driver
//!   does not expose a custom-stream injection API
//!   (`mysql_async`, `tiberius`, `rusqlite`, `oracle`).
//! - **`Stream`** — hands back a [`TunnelStream`] suitable for
//!   `tokio_postgres::Config::connect_raw`. Avoids the local TCP hop
//!   for Postgres specifically.

/// Resolved SSH tunnel configuration.
///
/// All fields have their defaults filled in by the merge step in
/// `ferrule-cli`, so consumers do not need to handle `Option`s or
/// env-var lookups when this value reaches the tunnel layer.
#[derive(Debug, Clone)]
pub struct SshConfig {
    /// SSH bastion hostname or IP.
    pub host: String,
    /// SSH server port. Defaulted to 22 by the merger when omitted.
    pub port: u16,
    /// SSH login username. Defaulted to `$USER` by the merger.
    pub user: String,
    /// Path to the SSH private key. `None` means resolve through the
    /// key stack (`~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, then
    /// `SSH_AUTH_SOCK`) at connect time.
    pub key_path: Option<String>,
}

#[cfg(feature = "ssh")]
mod ssh_impl {
    use super::SshConfig;
    use std::io;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Where the SSH session sources its private key from. The CLI's
    /// resolution stack collapses `--ssh-key`, profile entries,
    /// `FERRULE_<NAME>_SSH_KEY`, default identity files, and
    /// `SSH_AUTH_SOCK` into one of these variants before reaching
    /// [`setup_tunnel`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum KeySource {
        /// A private key file on disk. The russh layer loads and (if
        /// encrypted) decrypts it via [`russh::keys::load_secret_key`].
        File(PathBuf),
        /// SSH agent socket. The russh layer routes signing requests
        /// through the agent at this socket path.
        Agent(PathBuf),
    }

    /// Selects which transport [`setup_tunnel`] returns. See the
    /// module-level docs for when to pick each.
    #[derive(Debug, Clone, Copy)]
    pub enum TunnelTransport {
        /// Bind a local TCP listener; pump bytes through SSH.
        LocalListener,
        /// Hand back a [`TunnelStream`] for direct injection into a
        /// driver that exposes a custom-stream API (Postgres only
        /// today via `tokio_postgres::Config::connect_raw`).
        Stream,
    }

    /// Errors raised by the tunnel layer.
    ///
    /// `From<russh::Error>` is required by the `russh::client::Handler`
    /// associated `Error` bound, so the dedicated `Russh` variant is
    /// the conversion target — `Session`/`Auth`/`Key`/`Channel` are
    /// for diagnostics the tunnel layer raises itself.
    #[derive(Debug, thiserror::Error)]
    pub enum TunnelError {
        #[error("SSH session error: {0}")]
        Session(String),
        #[error("SSH authentication failed: {0}")]
        Auth(String),
        #[error("SSH key load error: {0}")]
        Key(String),
        #[error("SSH channel error: {0}")]
        Channel(String),
        #[error("russh error: {0}")]
        Russh(#[from] russh::Error),
        #[error("I/O error: {0}")]
        Io(#[from] io::Error),
    }

    /// `AsyncRead + AsyncWrite` wrapper around a russh direct-tcpip
    /// channel. Suitable for feeding into
    /// `tokio_postgres::Config::connect_raw`.
    pub struct TunnelStream {
        pub inner: russh::ChannelStream<russh::client::Msg>,
    }

    impl tokio::io::AsyncRead for TunnelStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for TunnelStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    /// Holds the russh session for the tunnel's lifetime. Dropping
    /// this terminates the session and tears down all channels using
    /// it — standard Rust ownership instead of an explicit close
    /// protocol.
    pub struct SshSession {
        pub handle: russh::client::Handle<ClientHandler>,
    }

    /// Outcome of [`setup_tunnel`]. The session is held alongside
    /// the transport-specific resources so callers only need to
    /// keep one value alive — when [`TunnelHandle`] drops, the SSH
    /// session and (for path a) the forwarder task drop with it.
    ///
    /// [`SshSession`] is hoisted out of [`TunnelTransport`] (which
    /// would otherwise carry it in both variants) to keep the
    /// transport enum's variants small — `russh::client::Handle`
    /// is hundreds of bytes and would trip
    /// `clippy::large_enum_variant` if duplicated per variant.
    pub struct TunnelHandle {
        pub session: SshSession,
        pub transport: TunnelTransportResult,
    }

    /// Transport-specific resources returned alongside the SSH
    /// session.
    pub enum TunnelTransportResult {
        /// (a) Local TCP listener path. Point the existing driver at
        /// `127.0.0.1:port`; `forwarder` pumps bytes between the
        /// listener and a russh direct-tcpip channel.
        LocalPort {
            port: u16,
            forwarder: tokio::task::JoinHandle<()>,
        },
        /// (b) Direct stream path. Hand `stream` to a driver that
        /// accepts a pre-built `AsyncRead + AsyncWrite + Unpin + Send
        /// + 'static` (Postgres via `connect_raw`).
        ///
        /// Boxed so the enum's variants stay roughly the same size —
        /// `TunnelStream` wraps a `russh::ChannelStream` whose
        /// internals (channels, JoinHandles) make it large compared
        /// to the `LocalPort` variant.
        Stream { stream: Box<TunnelStream> },
    }

    /// Wraps a backend [`Connection`](crate::Connection) plus the
    /// SSH session (and, for the LocalListener transport, the
    /// forwarder task) so the entire stack drops together.
    ///
    /// Why this is non-generic: dispatch returns `Box<dyn
    /// Connection>` regardless of backend, so an outer wrapper that
    /// already holds the inner as `Box<dyn Connection>` saves us
    /// from adding a blanket `impl<C: Connection> Connection for
    /// TunneledConnection<C>` and the matching `impl<C> Connection
    /// for Box<C>` (which `async_trait` doesn't synthesize).
    pub struct TunneledConnection {
        pub inner: Box<dyn crate::Connection>,
        /// Held for `Drop` only — lifetime guard for the SSH session.
        pub session: SshSession,
        /// `Some` for the LocalListener transport, `None` for the
        /// Stream transport (Postgres feeds the stream directly into
        /// `tokio_postgres::Connection`'s task, no separate
        /// forwarder needed).
        pub forwarder: Option<tokio::task::JoinHandle<()>>,
    }

    #[async_trait::async_trait]
    impl crate::Connection for TunneledConnection {
        async fn execute(
            &mut self,
            sql: &str,
        ) -> Result<crate::ExecutionSummary, crate::CoreError> {
            self.inner.execute(sql).await
        }

        async fn query(
            &mut self,
            sql: &str,
        ) -> Result<crate::QueryResult, crate::CoreError> {
            self.inner.query(sql).await
        }

        async fn execute_multi(
            &mut self,
            sql: &str,
        ) -> Result<Vec<crate::StatementResult>, crate::CoreError> {
            self.inner.execute_multi(sql).await
        }

        async fn ping(&mut self) -> Result<(), crate::CoreError> {
            self.inner.ping().await
        }

        async fn list_tables(
            &mut self,
            schema: Option<&str>,
        ) -> Result<Vec<String>, crate::CoreError> {
            self.inner.list_tables(schema).await
        }

        async fn describe_table(
            &mut self,
            schema: Option<&str>,
            table: &str,
        ) -> Result<crate::QueryResult, crate::CoreError> {
            self.inner.describe_table(schema, table).await
        }
    }

    /// russh client handler.
    ///
    /// **Server-key verification:** [`Self::check_server_key`]
    /// returns `Ok(true)` for every key — i.e. blanket-accept. This
    /// is dev-quality only. Production should compare against
    /// `~/.ssh/known_hosts`; that is its own design discussion and
    /// is intentionally deferred. A one-line stderr warning is
    /// emitted on every tunnel setup so users notice.
    pub struct ClientHandler;

    impl russh::client::Handler for ClientHandler {
        type Error = TunnelError;

        async fn check_server_key(
            &mut self,
            _server_public_key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<bool, Self::Error> {
            // Blanket accept — see struct docstring. The known_hosts
            // policy is staged separately; remove this method body
            // and replace with a real comparison once that lands.
            Ok(true)
        }
    }

    /// Establish an SSH session and a direct-tcpip channel to
    /// `target_host:target_port`. Returns a [`TunnelHandle`] whose
    /// shape depends on `transport`.
    ///
    /// Auth flow:
    /// - [`KeySource::File`] — load via [`russh::keys::load_secret_key`]
    ///   (no passphrase support yet — encrypted keys error out with a
    ///   diagnostic), then `authenticate_publickey`. RSA hash
    ///   algorithm is auto-negotiated via
    ///   [`russh::client::Handle::best_supported_rsa_hash`] and
    ///   defaults to SHA-256 when the server doesn't advertise.
    /// - [`KeySource::Agent`] — connect to the agent socket, request
    ///   identities, try `authenticate_publickey_with` against each
    ///   public key (skipping certificate identities for now) until
    ///   one succeeds.
    pub async fn setup_tunnel(
        config: &SshConfig,
        key_source: &KeySource,
        target_host: &str,
        target_port: u16,
        transport: TunnelTransport,
    ) -> Result<TunnelHandle, TunnelError> {
        use russh::client;
        use russh::client::AuthResult;
        use russh::keys::agent::client::AgentClient;
        use russh::keys::agent::AgentIdentity;
        use russh::keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg};
        use std::sync::Arc;

        // Surface the dev-quality host-key policy on every tunnel
        // setup; matches the project's `--insecure` warning style.
        eprintln!(
            "[ferrule] Warning: SSH host key verification is disabled \
             (known_hosts comparison not yet implemented). Connecting to \
             {}:{} on faith.",
            config.host, config.port
        );

        let cfg = Arc::new(client::Config::default());
        let mut handle = client::connect(
            cfg,
            (config.host.as_str(), config.port),
            ClientHandler,
        )
        .await
        .map_err(|e| {
            TunnelError::Session(format!(
                "connect to {}:{}: {}",
                config.host, config.port, e
            ))
        })?;

        // RSA hash auto-negotiation. Server's advertised value wins;
        // fall back to SHA-256 (modern default) if the server didn't
        // send `server-sig-algs`. ed25519 / ecdsa keys ignore this.
        let rsa_hash = match handle.best_supported_rsa_hash().await {
            Ok(Some(advertised)) => advertised,
            Ok(None) | Err(_) => Some(HashAlg::Sha256),
        };

        match key_source {
            KeySource::File(path) => {
                let key = load_secret_key(path, None).map_err(|e| {
                    TunnelError::Key(format!(
                        "load SSH key from {}: {}",
                        path.display(),
                        e
                    ))
                })?;
                let auth = handle
                    .authenticate_publickey(
                        &config.user,
                        PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
                    )
                    .await?;
                if !auth.success() {
                    return Err(TunnelError::Auth(format!(
                        "publickey auth failed for user '{}' (server rejected key {})",
                        config.user,
                        path.display()
                    )));
                }
            }
            KeySource::Agent(sock_path) => {
                let mut agent =
                    AgentClient::connect_uds(sock_path).await.map_err(|e| {
                        TunnelError::Auth(format!(
                            "connect to SSH agent at {}: {}",
                            sock_path.display(),
                            e
                        ))
                    })?;
                let identities = agent.request_identities().await.map_err(|e| {
                    TunnelError::Auth(format!("agent request_identities: {}", e))
                })?;
                if identities.is_empty() {
                    return Err(TunnelError::Auth(format!(
                        "SSH agent at {} has no identities loaded",
                        sock_path.display()
                    )));
                }
                let mut authed = false;
                let mut last_err: Option<String> = None;
                for ident in &identities {
                    let pk = match ident {
                        AgentIdentity::PublicKey { key, .. } => key.clone(),
                        // Certificate identities require a different
                        // auth call (`authenticate_certificate_with`);
                        // skip for now to keep commit B narrow.
                        AgentIdentity::Certificate { .. } => continue,
                    };
                    match handle
                        .authenticate_publickey_with(
                            &config.user,
                            pk,
                            rsa_hash,
                            &mut agent,
                        )
                        .await
                    {
                        Ok(AuthResult::Success) => {
                            authed = true;
                            break;
                        }
                        Ok(AuthResult::Failure { .. }) => continue,
                        Err(e) => {
                            last_err = Some(format!("{:?}", e));
                        }
                    }
                }
                if !authed {
                    return Err(TunnelError::Auth(format!(
                        "agent publickey auth failed for user '{}' \
                         (all {} identit{} rejected{})",
                        config.user,
                        identities.len(),
                        if identities.len() == 1 { "y" } else { "ies" },
                        last_err
                            .map(|e| format!(": last error: {e}"))
                            .unwrap_or_default(),
                    )));
                }
            }
        }

        let channel = handle
            .channel_open_direct_tcpip(
                target_host,
                u32::from(target_port),
                "127.0.0.1",
                0,
            )
            .await
            .map_err(|e| {
                TunnelError::Channel(format!(
                    "direct-tcpip to {}:{}: {}",
                    target_host, target_port, e
                ))
            })?;

        let session = SshSession { handle };

        match transport {
            TunnelTransport::Stream => Ok(TunnelHandle {
                session,
                transport: TunnelTransportResult::Stream {
                    stream: Box::new(TunnelStream {
                        inner: channel.into_stream(),
                    }),
                },
            }),
            TunnelTransport::LocalListener => {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let port = listener.local_addr()?.port();
                let forwarder = tokio::spawn(async move {
                    // Accept exactly one inbound connection — the
                    // database driver opens a single socket. If the
                    // driver retries (rare in our one-shot CLI), the
                    // user re-runs the command to set up a fresh
                    // tunnel.
                    let (mut tcp, _addr) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(e) => {
                            eprintln!(
                                "[ferrule] SSH tunnel listener accept failed: {}",
                                e
                            );
                            return;
                        }
                    };
                    let mut ssh = channel.into_stream();
                    if let Err(e) =
                        tokio::io::copy_bidirectional(&mut tcp, &mut ssh).await
                    {
                        eprintln!(
                            "[ferrule] SSH tunnel forwarder closed: {}",
                            e
                        );
                    }
                });
                Ok(TunnelHandle {
                    session,
                    transport: TunnelTransportResult::LocalPort { port, forwarder },
                })
            }
        }
    }
}

#[cfg(feature = "ssh")]
pub use ssh_impl::{
    setup_tunnel, ClientHandler, KeySource, SshSession, TunnelError, TunnelHandle, TunnelStream,
    TunnelTransport, TunnelTransportResult, TunneledConnection,
};
