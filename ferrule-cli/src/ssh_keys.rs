//! SSH key resolution stack — mirrors the password resolution stack
//! at `ferrule-config/src/credentials.rs` and project-wide convention.
//!
//! Resolution order (first hit wins):
//!
//! 1. Explicit hint from `--ssh-key` CLI flag or `ssh_key` profile key
//!    (these are already collapsed by [`crate::ssh_flags::merge_ssh_config`]).
//! 2. `FERRULE_<NAME>_SSH_KEY` env var.
//! 3. `~/.ssh/id_ed25519`.
//! 4. `~/.ssh/id_rsa`.
//! 5. `SSH_AUTH_SOCK` (SSH agent).
//! 6. Fail with diagnostic.
//!
//! The result is a [`KeySource`] enum that the russh tunnel layer
//! consumes to either load a key file (decrypting via passphrase
//! prompt if encrypted) or talk to the agent socket.

use crate::error::CliError;
use std::path::{Path, PathBuf};

/// Where the SSH session sources its private key from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A private key file on disk. The russh layer loads and (if
    /// encrypted) decrypts it.
    File(PathBuf),
    /// The SSH agent socket at this path. The russh layer routes
    /// signing requests through the agent.
    Agent(PathBuf),
}

/// Conversion to the core-side type that `setup_tunnel` consumes.
/// Same shape; the duplication exists because the cli-side enum is
/// always compiled while the core-side enum is gated behind the
/// `ssh` feature.
#[cfg(feature = "ssh")]
impl From<KeySource> for ferrule_core::KeySource {
    fn from(src: KeySource) -> ferrule_core::KeySource {
        match src {
            KeySource::File(p) => ferrule_core::KeySource::File(p, None),
            KeySource::Agent(p) => ferrule_core::KeySource::Agent(p),
        }
    }
}

/// Resolve the SSH key source. Pure logic; all environmental inputs
/// are passed in explicitly so tests can control them without env
/// races against parallel test threads.
///
/// - `connection_name`: connection identifier; used to build the
///   `FERRULE_<NAME>_SSH_KEY` env var name (uppercased, `-` → `_`).
/// - `key_hint`: merged result of `--ssh-key` flag and `profile.ssh_key`;
///   takes precedence over everything else.
/// - `home_dir`: usually `dirs::home_dir()`. Tests inject a temp dir.
/// - `ssh_auth_sock`: usually the value of the `SSH_AUTH_SOCK` env var.
///   Empty strings are treated as unset (matches OpenSSH convention).
/// - `env_key_var`: usually `std::env::var(...)` of the FERRULE env name.
///   Tests inject directly.
pub fn resolve_key_source(
    connection_name: &str,
    key_hint: Option<&str>,
    home_dir: Option<&Path>,
    ssh_auth_sock: Option<&str>,
    env_key_var: Option<&str>,
) -> Result<KeySource, CliError> {
    // Step 1+2: explicit hint from CLI flag or profile.
    if let Some(raw) = key_hint {
        let path = expand_tilde(raw, home_dir);
        if path.exists() {
            return Ok(KeySource::File(path));
        }
        return Err(CliError::usage(format!(
            "SSH key '{}' (from --ssh-key or profile.ssh_key) was not found",
            path.display()
        )));
    }

    // Step 3: FERRULE_<NAME>_SSH_KEY env var.
    if let Some(raw) = env_key_var {
        if !raw.is_empty() {
            let path = expand_tilde(raw, home_dir);
            if path.exists() {
                return Ok(KeySource::File(path));
            }
            let env_name = format_env_var_name(connection_name);
            return Err(CliError::usage(format!(
                "{}='{}' but key file not found at {}",
                env_name,
                raw,
                path.display()
            )));
        }
    }

    // Step 4+5: default identity files in ~/.ssh.
    if let Some(home) = home_dir {
        for default in DEFAULT_IDENTITY_FILES {
            let path = home.join(".ssh").join(default);
            if path.exists() {
                return Ok(KeySource::File(path));
            }
        }
    }

    // Step 6: SSH agent.
    if let Some(sock) = ssh_auth_sock {
        if !sock.is_empty() {
            return Ok(KeySource::Agent(PathBuf::from(sock)));
        }
    }

    // Step 7: fail with a diagnostic listing every option the user has.
    let env_name = format_env_var_name(connection_name);
    Err(CliError::usage(format!(
        "no SSH key resolved for connection '{}'. Provide one of:\n\
         \x20 --ssh-key <path>\n\
         \x20 ssh_key in the profile\n\
         \x20 {}=<path> env var\n\
         \x20 ~/.ssh/id_ed25519 or ~/.ssh/id_rsa identity file\n\
         \x20 a running SSH agent (SSH_AUTH_SOCK)",
        connection_name, env_name
    )))
}

/// Convenience wrapper: look up `home_dir`, `SSH_AUTH_SOCK`, and the
/// per-connection env var from the actual environment, then call
/// [`resolve_key_source`].
pub fn resolve_key_source_default(
    connection_name: &str,
    key_hint: Option<&str>,
) -> Result<KeySource, CliError> {
    let home = dirs::home_dir();
    let sock = std::env::var("SSH_AUTH_SOCK").ok();
    let env_name = format_env_var_name(connection_name);
    let env_val = std::env::var(&env_name).ok();
    resolve_key_source(
        connection_name,
        key_hint,
        home.as_deref(),
        sock.as_deref(),
        env_val.as_deref(),
    )
}

const DEFAULT_IDENTITY_FILES: &[&str] = &["id_ed25519", "id_rsa"];

fn format_env_var_name(connection_name: &str) -> String {
    format!(
        "FERRULE_{}_SSH_KEY",
        connection_name.to_ascii_uppercase().replace('-', "_")
    )
}

fn expand_tilde(path: &str, home_dir: Option<&Path>) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/") {
        if let Some(home) = home_dir {
            return home.join(stripped);
        }
    } else if path == "~" {
        if let Some(home) = home_dir {
            return home.to_path_buf();
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Returns a fresh temp directory for an SSH-key resolution test.
    /// Each call yields a unique path so concurrent tests do not
    /// collide. Pattern matches `ferrule-core/src/backends/sqlite.rs`.
    fn fresh_test_home() -> PathBuf {
        let pid = std::process::id();
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("ferrule-sshkey-test-{pid}-{n}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn explicit_hint_existing_file() {
        let home = fresh_test_home();
        let key = home.join("my-key");
        touch(&key);
        let resolved =
            resolve_key_source("x", Some(key.to_str().unwrap()), None, None, None).unwrap();
        assert_eq!(resolved, KeySource::File(key));
    }

    #[test]
    fn explicit_hint_missing_file_errors() {
        let err = resolve_key_source("x", Some("/nonexistent/key"), None, None, None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn explicit_hint_with_tilde_expands_against_home() {
        let home = fresh_test_home();
        let key = home.join(".ssh").join("custom_key");
        touch(&key);
        let resolved =
            resolve_key_source("x", Some("~/.ssh/custom_key"), Some(&home), None, None).unwrap();
        assert_eq!(resolved, KeySource::File(key));
    }

    #[test]
    fn env_var_existing_file_when_no_hint() {
        let home = fresh_test_home();
        let key = home.join("env-key");
        touch(&key);
        let resolved =
            resolve_key_source("x", None, None, None, Some(key.to_str().unwrap())).unwrap();
        assert_eq!(resolved, KeySource::File(key));
    }

    #[test]
    fn env_var_missing_file_errors_with_var_name() {
        let err =
            resolve_key_source("my-conn", None, None, None, Some("/nonexistent/path")).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("FERRULE_MY_CONN_SSH_KEY"));
        assert!(s.contains("/nonexistent/path"));
    }

    #[test]
    fn default_id_ed25519_picked_when_no_hint_or_env() {
        let home = fresh_test_home();
        let key = home.join(".ssh").join("id_ed25519");
        touch(&key);
        let resolved = resolve_key_source("x", None, Some(&home), None, None).unwrap();
        assert_eq!(resolved, KeySource::File(key));
    }

    #[test]
    fn default_id_rsa_picked_when_id_ed25519_absent() {
        let home = fresh_test_home();
        let key = home.join(".ssh").join("id_rsa");
        touch(&key);
        let resolved = resolve_key_source("x", None, Some(&home), None, None).unwrap();
        assert_eq!(resolved, KeySource::File(key));
    }

    #[test]
    fn id_ed25519_preferred_over_id_rsa() {
        let home = fresh_test_home();
        let ed = home.join(".ssh").join("id_ed25519");
        let rsa = home.join(".ssh").join("id_rsa");
        touch(&ed);
        touch(&rsa);
        let resolved = resolve_key_source("x", None, Some(&home), None, None).unwrap();
        assert_eq!(resolved, KeySource::File(ed));
    }

    #[test]
    fn ssh_auth_sock_used_when_no_files() {
        let home = fresh_test_home(); // empty ~/.ssh
        let resolved = resolve_key_source(
            "x",
            None,
            Some(&home),
            Some("/run/user/1000/ssh-agent.sock"),
            None,
        )
        .unwrap();
        assert_eq!(
            resolved,
            KeySource::Agent(PathBuf::from("/run/user/1000/ssh-agent.sock"))
        );
    }

    #[test]
    fn empty_ssh_auth_sock_treated_as_unset() {
        let home = fresh_test_home();
        let err = resolve_key_source("x", None, Some(&home), Some(""), None).unwrap_err();
        assert!(err.to_string().contains("no SSH key resolved"));
    }

    #[test]
    fn nothing_anywhere_errors_with_full_diagnostic() {
        let home = fresh_test_home();
        let err = resolve_key_source("prod-pg", None, Some(&home), None, None).unwrap_err();
        let s = err.to_string();
        assert!(s.contains("prod-pg"));
        assert!(s.contains("--ssh-key"));
        assert!(s.contains("FERRULE_PROD_PG_SSH_KEY"));
        assert!(s.contains("id_ed25519"));
        assert!(s.contains("SSH_AUTH_SOCK"));
    }

    #[test]
    fn precedence_hint_beats_env() {
        let home = fresh_test_home();
        let hint_key = home.join("hint");
        let env_key = home.join("env");
        touch(&hint_key);
        touch(&env_key);
        let resolved = resolve_key_source(
            "x",
            Some(hint_key.to_str().unwrap()),
            None,
            None,
            Some(env_key.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(resolved, KeySource::File(hint_key));
    }

    #[test]
    fn precedence_env_beats_default_identity() {
        let home = fresh_test_home();
        let id_ed = home.join(".ssh").join("id_ed25519");
        touch(&id_ed);
        let env_key = home.join("env");
        touch(&env_key);
        let resolved = resolve_key_source(
            "x",
            None,
            Some(&home),
            None,
            Some(env_key.to_str().unwrap()),
        )
        .unwrap();
        assert_eq!(resolved, KeySource::File(env_key));
    }

    #[test]
    fn precedence_default_identity_beats_agent() {
        let home = fresh_test_home();
        let id_ed = home.join(".ssh").join("id_ed25519");
        touch(&id_ed);
        let resolved =
            resolve_key_source("x", None, Some(&home), Some("/some/agent.sock"), None).unwrap();
        assert_eq!(resolved, KeySource::File(id_ed));
    }

    #[test]
    fn env_var_name_uppercases_and_replaces_dashes() {
        assert_eq!(format_env_var_name("prod-pg"), "FERRULE_PROD_PG_SSH_KEY");
        assert_eq!(format_env_var_name("staging"), "FERRULE_STAGING_SSH_KEY");
        assert_eq!(format_env_var_name("a-b-c-d"), "FERRULE_A_B_C_D_SSH_KEY");
    }
}
