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

    /// Wraps an inner backend connection plus the [`TunnelHandle`].
    /// The SSH session and (for path a) the forwarder task share the
    /// connection's lifetime: dropping the [`TunneledConnection`]
    /// drops everything in tandem.
    ///
    /// Wave 3 B3 step 2c only adds the wrapper struct. The
    /// blanket `Connection` impl (delegating every method to
    /// `inner`) lands with the dispatch wiring in Commit C.
    pub struct TunneledConnection<C> {
        pub inner: C,
        pub handle: TunnelHandle,
    }

    /// Internal russh client handler. Server-key verification policy
    /// (currently trust-on-first-use; production should compare
    /// against `~/.ssh/known_hosts`) lives here. The handler is
    /// fleshed out in Commit B; for now it provides only the
    /// associated `Error` type so [`SshSession`] has a concrete
    /// `Handle<ClientHandler>` to hold.
    pub struct ClientHandler;

    impl russh::client::Handler for ClientHandler {
        type Error = TunnelError;
        // All methods use the trait defaults until Commit B adds
        // explicit `check_server_key` and (optionally) auth-event
        // hooks. The default `check_server_key` rejects every key,
        // which is fine because `setup_tunnel` is `unimplemented!()`
        // — no caller can invoke the handler until Commit B.
    }

    /// Establish an SSH session and a direct-tcpip channel to
    /// `target_host:target_port`. Returns a [`TunnelHandle`] whose
    /// shape depends on `transport`.
    ///
    /// **Stub:** the russh wiring lands in Commit B of Wave 3 B3
    /// step 2c. The CLI dispatch path (Commit C) is the only
    /// caller, so callers in commits A/B see no observable
    /// difference.
    pub async fn setup_tunnel(
        config: &SshConfig,
        key_source: &KeySource,
        target_host: &str,
        target_port: u16,
        transport: TunnelTransport,
    ) -> Result<TunnelHandle, TunnelError> {
        let _ = (config, key_source, target_host, target_port, transport);
        unimplemented!("setup_tunnel — russh wiring lands in Commit B")
    }
}

#[cfg(feature = "ssh")]
pub use ssh_impl::{
    setup_tunnel, ClientHandler, KeySource, SshSession, TunnelError, TunnelHandle, TunnelStream,
    TunnelTransport, TunnelTransportResult, TunneledConnection,
};
