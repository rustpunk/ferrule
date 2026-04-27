//! Parsing and merging of SSH tunnel CLI flags with profile config.
//!
//! The CLI exposes two flags:
//!
//! - `--ssh-tunnel [user@]host[:port]` (matches pgcli's syntax verbatim)
//! - `--ssh-key <path>`
//!
//! Profiles in `.ferrule.toml` expose four keys: `ssh_host`, `ssh_user`,
//! `ssh_port`, `ssh_key`. The merger takes both inputs and produces a
//! single resolved [`SshConfig`] (or `None` when no SSH bits are set
//! anywhere). CLI flags override the corresponding profile keys; the
//! `--ssh-tunnel` flag overrides `ssh_host` / `ssh_user` / `ssh_port`
//! atomically (matching pgcli's "one flag, one tunnel target" pattern).

use crate::error::CliError;
use ferrule_config::profile::{ConnectionProfile, GlobalConfig};
use ferrule_core::tunnel::SshConfig;

/// Result of parsing a `--ssh-tunnel [user@]host[:port]` flag value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSshTunnel {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// Parse a `--ssh-tunnel` CLI flag value.
///
/// Accepted forms:
/// - `host`
/// - `host:port`
/// - `user@host`
/// - `user@host:port`
///
/// IPv6 addresses must be bracketed (e.g. `user@[::1]:22`); unbracketed
/// IPv6 is not supported because `:` would be ambiguous with the port
/// separator.
pub fn parse_ssh_tunnel(raw: &str) -> Result<ParsedSshTunnel, CliError> {
    if raw.is_empty() {
        return Err(CliError::usage(
            "--ssh-tunnel value must not be empty".to_string(),
        ));
    }

    // Split user@host[:port] on the rightmost '@'. The user portion may
    // legitimately contain '@' (rare but possible), so we take the last.
    let (user, host_port) = match raw.rsplit_once('@') {
        Some((u, hp)) if !u.is_empty() => (Some(u.to_string()), hp),
        Some(_) => {
            return Err(CliError::usage(format!(
                "--ssh-tunnel: empty user before '@' in '{raw}'"
            )));
        }
        None => (None, raw),
    };

    if host_port.is_empty() {
        return Err(CliError::usage(format!(
            "--ssh-tunnel: empty host in '{raw}'"
        )));
    }

    // Split host:port on the rightmost ':'. For bracketed IPv6 like
    // `[::1]:22` this still gives us `[::1]` and `22`.
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => {
            let port = p.parse::<u16>().map_err(|e| {
                CliError::usage(format!(
                    "--ssh-tunnel: invalid port '{p}' in '{raw}': {e}"
                ))
            })?;
            (h.to_string(), Some(port))
        }
        Some((_, _)) => {
            return Err(CliError::usage(format!(
                "--ssh-tunnel: empty host before ':' in '{raw}'"
            )));
        }
        None => (host_port.to_string(), None),
    };

    Ok(ParsedSshTunnel { user, host, port })
}

/// Merge a connection profile and CLI flags into a resolved
/// [`SshConfig`].
///
/// Returns `Ok(None)` when neither the profile nor the CLI specifies
/// any SSH bits — the connection is plain (no tunnel).
///
/// Override rules:
/// - `--ssh-tunnel` overrides `ssh_host` / `ssh_user` / `ssh_port`
///   *atomically*. If you pass `--ssh-tunnel host` without a user, the
///   user falls back to `$USER` rather than the profile's `ssh_user` —
///   "one flag, one tunnel target."
/// - `--ssh-key` independently overrides `ssh_key`.
/// - Defaults: port → 22; user → `$USER` (or the literal "user" if
///   `$USER` is unset, which only happens in heavily sandboxed
///   environments).
pub fn merge_ssh_config(
    profile: Option<&ConnectionProfile>,
    cli_ssh_tunnel: Option<&str>,
    cli_ssh_key: Option<&str>,
) -> Result<Option<SshConfig>, CliError> {
    let cli_flag = cli_ssh_tunnel.map(parse_ssh_tunnel).transpose()?;

    // CLI flag wins; otherwise use profile fields.
    let host = cli_flag
        .as_ref()
        .map(|f| f.host.clone())
        .or_else(|| profile.and_then(|p| p.ssh_host.clone()));
    let Some(host) = host else {
        // No SSH bits anywhere → plain connection.
        return Ok(None);
    };

    let user = if let Some(flag) = &cli_flag {
        flag.user.clone()
    } else {
        profile.and_then(|p| p.ssh_user.clone())
    }
    .unwrap_or_else(default_ssh_user);

    let port = if let Some(flag) = &cli_flag {
        flag.port
    } else {
        profile.and_then(|p| p.ssh_port)
    }
    .unwrap_or(22);

    let key_path = cli_ssh_key
        .map(String::from)
        .or_else(|| profile.and_then(|p| p.ssh_key.clone()));

    Ok(Some(SshConfig {
        host,
        port,
        user,
        key_path,
    }))
}

/// Look up the connection's profile (if any) and merge it with CLI
/// flags into a final [`SshConfig`]. Returns `Ok(None)` when no SSH
/// bits are configured anywhere.
pub fn resolve_ssh_config(
    connection_name: &str,
    cli_ssh_tunnel: Option<&str>,
    cli_ssh_key: Option<&str>,
    global_config: &GlobalConfig,
) -> Result<Option<SshConfig>, CliError> {
    let profile = global_config.connection.get(connection_name);
    merge_ssh_config(profile, cli_ssh_tunnel, cli_ssh_key)
}

fn default_ssh_user() -> String {
    // `USER` on POSIX, `USERNAME` on Windows. Either being unset is rare
    // enough that "user" is an acceptable last-resort sentinel — better
    // to surface a clear authentication failure later than to refuse to
    // run at all.
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;

    fn empty_profile() -> ConnectionProfile {
        ConnectionProfile {
            url: "postgres://app@db/myapp".into(),
            password_url: None,
            headers: IndexMap::new(),
            ssh_host: None,
            ssh_user: None,
            ssh_port: None,
            ssh_key: None,
        }
    }

    fn full_profile() -> ConnectionProfile {
        ConnectionProfile {
            url: "postgres://app@db/myapp".into(),
            password_url: None,
            headers: IndexMap::new(),
            ssh_host: Some("bastion.example.com".into()),
            ssh_user: Some("ec2-user".into()),
            ssh_port: Some(2222),
            ssh_key: Some("/home/me/.ssh/id_ed25519".into()),
        }
    }

    // --- parse_ssh_tunnel ---

    #[test]
    fn parse_host_only() {
        let p = parse_ssh_tunnel("bastion.example.com").unwrap();
        assert_eq!(p.user, None);
        assert_eq!(p.host, "bastion.example.com");
        assert_eq!(p.port, None);
    }

    #[test]
    fn parse_host_and_port() {
        let p = parse_ssh_tunnel("bastion:2222").unwrap();
        assert_eq!(p.user, None);
        assert_eq!(p.host, "bastion");
        assert_eq!(p.port, Some(2222));
    }

    #[test]
    fn parse_user_and_host() {
        let p = parse_ssh_tunnel("ec2-user@bastion").unwrap();
        assert_eq!(p.user.as_deref(), Some("ec2-user"));
        assert_eq!(p.host, "bastion");
        assert_eq!(p.port, None);
    }

    #[test]
    fn parse_full_form() {
        let p = parse_ssh_tunnel("ec2-user@bastion.example.com:22").unwrap();
        assert_eq!(p.user.as_deref(), Some("ec2-user"));
        assert_eq!(p.host, "bastion.example.com");
        assert_eq!(p.port, Some(22));
    }

    #[test]
    fn parse_bracketed_ipv6() {
        let p = parse_ssh_tunnel("user@[::1]:22").unwrap();
        assert_eq!(p.user.as_deref(), Some("user"));
        assert_eq!(p.host, "[::1]");
        assert_eq!(p.port, Some(22));
    }

    #[test]
    fn parse_empty_value_errors() {
        assert!(parse_ssh_tunnel("").is_err());
    }

    #[test]
    fn parse_empty_user_errors() {
        let err = parse_ssh_tunnel("@bastion").unwrap_err();
        assert!(err.to_string().contains("empty user"));
    }

    #[test]
    fn parse_invalid_port_errors() {
        let err = parse_ssh_tunnel("bastion:not-a-number").unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn parse_port_out_of_range_errors() {
        // 70000 is > u16::MAX (65535).
        assert!(parse_ssh_tunnel("bastion:70000").is_err());
    }

    // --- merge_ssh_config ---

    #[test]
    fn merge_no_ssh_anywhere_returns_none() {
        let result = merge_ssh_config(Some(&empty_profile()), None, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn merge_no_profile_no_cli_returns_none() {
        let result = merge_ssh_config(None, None, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn merge_profile_only() {
        let cfg = merge_ssh_config(Some(&full_profile()), None, None)
            .unwrap()
            .expect("profile has ssh_host");
        assert_eq!(cfg.host, "bastion.example.com");
        assert_eq!(cfg.user, "ec2-user");
        assert_eq!(cfg.port, 2222);
        assert_eq!(cfg.key_path.as_deref(), Some("/home/me/.ssh/id_ed25519"));
    }

    #[test]
    fn merge_cli_tunnel_overrides_profile_atomically() {
        // Profile sets ec2-user@bastion.example.com:2222 with key.
        // CLI overrides with `bastion-cli` (no user, no port specified).
        // Atomic-replacement semantics: user falls back to $USER (NOT
        // ec2-user), port falls back to 22 (NOT 2222). Key stays from
        // profile because --ssh-key wasn't passed.
        std::env::set_var("USER", "alice");
        let cfg = merge_ssh_config(Some(&full_profile()), Some("bastion-cli"), None)
            .unwrap()
            .unwrap();
        assert_eq!(cfg.host, "bastion-cli");
        assert_eq!(cfg.user, "alice", "user falls back to $USER, not profile");
        assert_eq!(cfg.port, 22, "port falls back to 22, not profile");
        assert_eq!(
            cfg.key_path.as_deref(),
            Some("/home/me/.ssh/id_ed25519"),
            "key stays from profile because --ssh-key was not passed"
        );
        std::env::remove_var("USER");
    }

    #[test]
    fn merge_cli_full_form_overrides_profile() {
        let cfg = merge_ssh_config(
            Some(&full_profile()),
            Some("override-user@override-host:9999"),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(cfg.host, "override-host");
        assert_eq!(cfg.user, "override-user");
        assert_eq!(cfg.port, 9999);
    }

    #[test]
    fn merge_cli_key_overrides_profile_key() {
        let cfg = merge_ssh_config(Some(&full_profile()), None, Some("/tmp/cli-key"))
            .unwrap()
            .unwrap();
        assert_eq!(cfg.key_path.as_deref(), Some("/tmp/cli-key"));
        // host/user/port still come from profile because --ssh-tunnel
        // wasn't passed.
        assert_eq!(cfg.host, "bastion.example.com");
    }

    #[test]
    fn merge_cli_only_no_profile() {
        let cfg = merge_ssh_config(None, Some("u@h:42"), Some("/k"))
            .unwrap()
            .unwrap();
        assert_eq!(cfg.host, "h");
        assert_eq!(cfg.user, "u");
        assert_eq!(cfg.port, 42);
        assert_eq!(cfg.key_path.as_deref(), Some("/k"));
    }

    #[test]
    fn merge_partial_profile_defaults_port_22() {
        // Profile has only ssh_host; verify port defaults to 22 and
        // user falls back to $USER.
        std::env::set_var("USER", "bob");
        let mut p = empty_profile();
        p.ssh_host = Some("bastion".into());
        let cfg = merge_ssh_config(Some(&p), None, None).unwrap().unwrap();
        assert_eq!(cfg.host, "bastion");
        assert_eq!(cfg.port, 22);
        assert_eq!(cfg.user, "bob");
        assert!(cfg.key_path.is_none());
        std::env::remove_var("USER");
    }
}
