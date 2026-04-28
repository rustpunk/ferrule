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
    use secrecy::{ExposeSecret, SecretString};
    use std::io;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};

    /// Where the SSH session sources its private key from. The CLI's
    /// resolution stack collapses `--ssh-key`, profile entries,
    /// `FERRULE_<NAME>_SSH_KEY`, default identity files, and
    /// `SSH_AUTH_SOCK` into one of these variants before reaching
    /// [`setup_tunnel`].
    #[derive(Debug, Clone)]
    pub enum KeySource {
        /// A private key file on disk. The russh layer loads and (if
        /// encrypted) decrypts it via [`russh::keys::load_secret_key`].
        /// `None` means probe first and error if encrypted;
        /// `Some` means attempt decryption with the provided passphrase.
        File(PathBuf, Option<SecretString>),
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
        /// Host key on file matches the server's advertised key.
        #[error("The server key has changed at line {line}")]
        HostKeyMismatch { host: String, port: u16, line: usize },
        /// Host not present in known_hosts — TOFU prompt required.
        #[error(
            "The authenticity of host '{host}:{port}' can't be established.\n\
             {algorithm} key fingerprint is {fingerprint}."
        )]
        UnknownHost {
            host: String,
            port: u16,
            algorithm: String,
            fingerprint: String,
            key: russh::keys::ssh_key::PublicKey,
        },
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

    /// Outcome of comparing a server public key against
    /// `~/.ssh/known_hosts`.
    pub enum HostKeyStatus {
        /// Key matches an existing entry.
        Match,
        /// Host is present but the key differs (possible MITM).
        Mismatch { line: usize },
        /// Host is not present in known_hosts.
        Unknown,
    }

    /// Check `host:port` against the user's `~/.ssh/known_hosts`.
    pub fn check_host_key(
        host: &str,
        port: u16,
        pubkey: &russh::keys::ssh_key::PublicKey,
    ) -> Result<HostKeyStatus, TunnelError> {
        match russh::keys::check_known_hosts(host, port, pubkey) {
            Ok(true) => Ok(HostKeyStatus::Match),
            Ok(false) => Ok(HostKeyStatus::Unknown),
            Err(russh::keys::Error::KeyChanged { line }) => Ok(HostKeyStatus::Mismatch { line }),
            Err(e) => Err(TunnelError::Session(format!(
                "known_hosts check for {host}:{port}: {e}"
            ))),
        }
    }

    /// Write a host's public key into `~/.ssh/known_hosts` (TOFU).
    pub fn learn_host_key(
        host: &str,
        port: u16,
        pubkey: &russh::keys::ssh_key::PublicKey,
    ) -> Result<(), TunnelError> {
        russh::keys::known_hosts::learn_known_hosts(host, port, pubkey).map_err(|e| {
            TunnelError::Session(format!(
                "failed to write host key to ~/.ssh/known_hosts: {e}"
            ))
        })
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
        pub handle: std::sync::Arc<
            tokio::sync::Mutex<russh::client::Handle<ClientHandler>>,
        >,
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
    /// [`check_server_key`] compares the server's public key against
    /// the user's `~/.ssh/known_hosts` via russh's native parser.
    /// Match → silent accept; mismatch → fatal error; unknown →
    /// `Err(TunnelError::UnknownHost)` so the CLI layer can prompt
    /// for TOFU and retry.
    pub struct ClientHandler {
        pub host: String,
        pub port: u16,
    }

    impl russh::client::Handler for ClientHandler {
        type Error = TunnelError;

        async fn check_server_key(
            &mut self,
            server_public_key: &russh::keys::ssh_key::PublicKey,
        ) -> Result<bool, Self::Error> {
            match check_host_key(&self.host,
                         self.port,
                         server_public_key,
            )? {
                HostKeyStatus::Match => Ok(true),
                HostKeyStatus::Mismatch { line } => {
                    Err(TunnelError::HostKeyMismatch {
                        host: self.host.clone(),
                        port: self.port,
                        line,
                    })
                }
                HostKeyStatus::Unknown => {
                    let fingerprint = server_public_key
                        .fingerprint(russh::keys::ssh_key::HashAlg::Sha256)
                        .to_string();
                    Err(TunnelError::UnknownHost {
                        host: self.host.clone(),
                        port: self.port,
                        algorithm: server_public_key.algorithm().to_string(),
                        fingerprint,
                        key: server_public_key.clone(),
                    })
                }
            }
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
        proxy: Option<&crate::proxy::ProxyConfig>,
    ) -> Result<TunnelHandle, TunnelError> {
        use russh::client;
        use russh::client::AuthResult;
        use russh::keys::agent::client::AgentClient;
        use russh::keys::agent::AgentIdentity;
        use russh::keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg};

        let cfg = Arc::new(client::Config::default());
        let mut handle = if let Some(proxy) = proxy {
            let proxy_stream = crate::proxy::http_connect(proxy, &config.host, config.port)
                .await
                .map_err(|e| TunnelError::Session(format!("proxy: {e}")))?;
            client::connect_stream(
                cfg,
                proxy_stream,
                ClientHandler {
                    host: config.host.clone(),
                    port: config.port,
                },
            )
            .await?
        } else {
            client::connect(
                cfg,
                (config.host.as_str(), config.port),
                ClientHandler {
                    host: config.host.clone(),
                    port: config.port,
                },
            )
            .await
            .map_err(|e| match e {
                TunnelError::HostKeyMismatch { .. } | TunnelError::UnknownHost { .. } => e,
                other => TunnelError::Session(format!(
                    "connect to {}:{}: {}",
                    config.host, config.port, other
                )),
            })?
        };

        // RSA hash auto-negotiation. Server's advertised value wins;
        // fall back to SHA-256 (modern default) if the server didn't
        // send `server-sig-algs`. ed25519 / ecdsa keys ignore this.
        let rsa_hash = match handle.best_supported_rsa_hash().await {
            Ok(Some(advertised)) => advertised,
            Ok(None) | Err(_) => Some(HashAlg::Sha256),
        };

        match key_source {
            KeySource::File(path, passphrase) => {
                let key = load_secret_key(path, passphrase.as_ref().map(|s| s.expose_secret())).map_err(|e| {
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

        // Wrap the authenticated handle in Arc<Mutex<>> so the
        // LocalListener forwarder can open fresh direct-tcpip
        // channels for each accepted connection while the Stream
        // path opens a single channel upfront.
        let handle = Arc::new(tokio::sync::Mutex::new(handle));
        let session = SshSession {
            handle: Arc::clone(&handle),
        };

        match transport {
            TunnelTransport::Stream => {
                let channel = handle
                    .lock()
                    .await
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
                Ok(TunnelHandle {
                    session,
                    transport: TunnelTransportResult::Stream {
                        stream: Box::new(TunnelStream {
                            inner: channel.into_stream(),
                        }),
                    },
                })
            }
            TunnelTransport::LocalListener => {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let port = listener.local_addr()?.port();
                let target_host = target_host.to_string();
                let handle = Arc::clone(&handle);
                let forwarder = tokio::spawn(async move {
                    loop {
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
                        let handle = Arc::clone(&handle);
                        let target_host = target_host.clone();
                        tokio::spawn(async move {
                            let guard = handle.lock().await;
                            let channel = match guard.channel_open_direct_tcpip(
                                &target_host,
                                u32::from(target_port),
                                "127.0.0.1",
                                0,
                            )
                            .await
                            {
                                Ok(ch) => ch,
                                Err(e) => {
                                    eprintln!(
                                        "[ferrule] SSH tunnel direct-tcpip failed: {}",
                                        e
                                    );
                                    return;
                                }
                            };
                            drop(guard);
                            let mut ssh = channel.into_stream();
                            if let Err(e) =
                                tokio::io::copy_bidirectional(&mut tcp, &mut ssh).await
                            {
                                // Normal close is expected; don't spam stderr.
                                let _ = e;
                            }
                        });
                    }
                });
                Ok(TunnelHandle {
                    session,
                    transport: TunnelTransportResult::LocalPort { port, forwarder },
                })
            }
        }
    }

    /// Probe whether an SSH private key file requires a passphrase.
    ///
    /// Returns `Ok(true)` if the key is encrypted, `Ok(false)` if it
    /// loads without a passphrase, and `Err(TunnelError::Key(...))`
    /// for I/O or parse errors.
    pub fn ssh_key_needs_passphrase(path: impl AsRef<std::path::Path>) -> Result<bool, TunnelError> {
        match russh::keys::load_secret_key(path.as_ref(), None) {
            Ok(_) => Ok(false),
            Err(russh::keys::Error::KeyIsEncrypted) => Ok(true),
            Err(e) => Err(TunnelError::Key(format!(
                "load SSH key from {}: {}",
                path.as_ref().display(),
                e
            ))),
        }
    }
}

#[cfg(feature = "ssh")]
pub use ssh_impl::{
    check_host_key, learn_host_key, setup_tunnel, ssh_key_needs_passphrase, ClientHandler,
    KeySource, SshSession, TunnelError, TunnelHandle, TunnelStream, TunnelTransport,
    TunnelTransportResult, TunneledConnection,
};

#[cfg(feature = "ssh")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_key_needs_passphrase_unencrypted() {
        let path = std::path::PathBuf::from("/tmp/ferrule-test-unencrypted");
        if path.exists() {
            assert!(!ssh_key_needs_passphrase(&path).unwrap());
        }
    }

    #[test]
    fn ssh_key_needs_passphrase_encrypted() {
        let path = std::path::PathBuf::from("/tmp/ferrule-test-encrypted");
        if path.exists() {
            assert!(ssh_key_needs_passphrase(&path).unwrap());
        }
    }
}
