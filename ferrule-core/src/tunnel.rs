//! SSH tunnel support — types and lifecycle.
//!
//! `SshConfig` is the validated output of merging profile keys and CLI
//! flags. It is the type backends consume to set up a tunnel before
//! opening their underlying connection.
//!
//! The actual tunnel implementation (russh session, port forwarding,
//! `TunneledConnection<C>` wrapper) lives behind the `ssh` Cargo feature
//! and is added in a follow-up commit.

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
