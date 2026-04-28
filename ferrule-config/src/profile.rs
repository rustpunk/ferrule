use crate::error::ConfigError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Global configuration + per-connection profiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    #[serde(default)]
    pub default: DefaultProfile,
    #[serde(default)]
    pub connection: IndexMap<String, ConnectionProfile>,
}

impl GlobalConfig {
    /// Load configuration using the standard discovery order:
    /// 1. Explicit path (if provided)
    /// 2. `./.ferrule.toml`
    /// 3. `~/.config/ferrule/ferrule.toml` (platform appropriate)
    pub fn load(explicit_path: Option<&str>) -> Result<Self, ConfigError> {
        let path = if let Some(p) = explicit_path {
            std::path::PathBuf::from(p)
        } else {
            Self::find_config_path()?
        };
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from(&path)
    }

    /// Load from a specific file path.
    pub fn load_from(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::ConfigNotFound(format!("{}: {}", path.display(), e)))?;
        let mut config: GlobalConfig =
            toml::from_str(&content).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        // Apply env interpolation to profile URLs and SSH config strings.
        for profile in config.connection.values_mut() {
            profile.url = crate::registry::interpolate_env_vars(&profile.url);
            if let Some(host) = &profile.ssh_host {
                profile.ssh_host = Some(crate::registry::interpolate_env_vars(host));
            }
            if let Some(user) = &profile.ssh_user {
                profile.ssh_user = Some(crate::registry::interpolate_env_vars(user));
            }
            if let Some(key) = &profile.ssh_key {
                profile.ssh_key = Some(crate::registry::interpolate_env_vars(key));
            }
        }
        Ok(config)
    }

    fn find_config_path() -> Result<std::path::PathBuf, ConfigError> {
        // 1. Project-local
        if let Ok(cwd) = std::env::current_dir() {
            let local = cwd.join(".ferrule.toml");
            if local.exists() {
                return Ok(local);
            }
        }
        // 2. User-global
        let config_dir = dirs::config_dir()
            .ok_or_else(|| {
                ConfigError::ConfigNotFound("could not determine config directory".into())
            })?
            .join("ferrule");
        Ok(config_dir.join("ferrule.toml"))
    }

    /// Resolve default output format, preferring explicit CLI value.
    pub fn resolve_format(&self, cli: Option<&str>) -> String {
        cli.map(|s| s.to_string())
            .unwrap_or_else(|| self.default.format.clone())
    }

    /// Resolve default limit, preferring explicit CLI value.
    pub fn resolve_limit(&self, cli: Option<usize>) -> Option<usize> {
        cli.or(self.default.limit_checked())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultProfile {
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

impl DefaultProfile {
    /// Returns `Some(limit)` if non-zero, otherwise `None` (unlimited).
    pub fn limit_checked(&self) -> Option<usize> {
        if self.limit == 0 {
            None
        } else {
            Some(self.limit)
        }
    }
}

impl Default for DefaultProfile {
    fn default() -> Self {
        Self {
            format: default_format(),
            limit: default_limit(),
            timeout: default_timeout(),
        }
    }
}

fn default_format() -> String {
    "json".to_string()
}

fn default_limit() -> usize {
    1000
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub url: String,
    #[serde(default)]
    pub password_url: Option<String>,
    #[serde(default)]
    pub headers: IndexMap<String, String>,

    /// SSH bastion hostname or IP. When set, ferrule opens an SSH session
    /// to this host and forwards a local port to `url`'s host:port. The
    /// rest of the `ssh_*` keys configure the SSH session.
    #[serde(default)]
    pub ssh_host: Option<String>,
    /// SSH login username. Defaults to `$USER` at connect time.
    #[serde(default)]
    pub ssh_user: Option<String>,
    /// SSH server port. Defaults to 22.
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// Path to the SSH private key. Tilde and `${VAR}` expansion happens
    /// at connect time. When `None`, the key is resolved through the key
    /// stack (CLI flag → env → default identity files → SSH agent).
    #[serde(default)]
    pub ssh_key: Option<String>,

    /// HTTP CONNECT proxy URL (e.g. `http://proxy:8080`).
    #[serde(default)]
    pub proxy_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_load_global_config_defaults() {
        let config = GlobalConfig::load(Some("/nonexistent/path.toml")).unwrap();
        assert_eq!(config.default.format, "json");
        assert_eq!(config.default.limit, 1000);
        assert_eq!(config.default.timeout, 30);
        assert!(config.connection.is_empty());
    }

    #[test]
    fn test_load_global_config_from_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"
[default]
format = "table"
limit = 500
timeout = 60

[connection.production]
url = "postgres://user:pass@host/db"
"#;
        tmp.write_all(content.as_bytes()).unwrap();
        let config = GlobalConfig::load_from(tmp.path()).unwrap();
        assert_eq!(config.default.format, "table");
        assert_eq!(config.default.limit, 500);
        assert_eq!(config.default.timeout, 60);
        assert_eq!(config.connection.len(), 1);
        let prod = config.connection.get("production").unwrap();
        assert_eq!(prod.url, "postgres://user:pass@host/db");
    }

    #[test]
    fn test_resolve_format_and_limit() {
        let mut config = GlobalConfig::default();
        config.default.format = "csv".into();
        config.default.limit = 50;

        assert_eq!(config.resolve_format(None), "csv");
        assert_eq!(config.resolve_format(Some("json")), "json");
        assert_eq!(config.resolve_limit(None), Some(50));
        assert_eq!(config.resolve_limit(Some(10)), Some(10));
    }

    #[test]
    fn test_env_interpolation_in_profile_url() {
        std::env::set_var("FERRULE_TEST_PROFILE_HOST", "myhost");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"
[connection.test]
url = "postgres://user@${FERRULE_TEST_PROFILE_HOST}/db"
"#;
        tmp.write_all(content.as_bytes()).unwrap();
        let config = GlobalConfig::load_from(tmp.path()).unwrap();
        let test = config.connection.get("test").unwrap();
        assert_eq!(test.url, "postgres://user@myhost/db");
        std::env::remove_var("FERRULE_TEST_PROFILE_HOST");
    }

    #[test]
    fn ssh_keys_default_to_none() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"
[connection.plain]
url = "postgres://user:pass@host/db"
"#;
        tmp.write_all(content.as_bytes()).unwrap();
        let config = GlobalConfig::load_from(tmp.path()).unwrap();
        let plain = config.connection.get("plain").unwrap();
        assert!(plain.ssh_host.is_none());
        assert!(plain.ssh_user.is_none());
        assert!(plain.ssh_port.is_none());
        assert!(plain.ssh_key.is_none());
    }

    #[test]
    fn ssh_keys_parse_when_present() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"
[connection.tunneled]
url = "postgres://app:pwd@10.0.0.5:5432/myapp"
ssh_host = "bastion.example.com"
ssh_user = "ec2-user"
ssh_port = 2222
ssh_key  = "/home/me/.ssh/id_ed25519"
"#;
        tmp.write_all(content.as_bytes()).unwrap();
        let config = GlobalConfig::load_from(tmp.path()).unwrap();
        let tunneled = config.connection.get("tunneled").unwrap();
        assert_eq!(tunneled.ssh_host.as_deref(), Some("bastion.example.com"));
        assert_eq!(tunneled.ssh_user.as_deref(), Some("ec2-user"));
        assert_eq!(tunneled.ssh_port, Some(2222));
        assert_eq!(tunneled.ssh_key.as_deref(), Some("/home/me/.ssh/id_ed25519"));
    }

    #[test]
    fn ssh_partial_keys_parse_independently() {
        // Only ssh_host set; the other ssh_* keys default to None and the
        // tunnel layer fills the gaps (ssh_user → $USER, ssh_port → 22,
        // ssh_key → resolved via key stack).
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"
[connection.minimal]
url = "postgres://app@db-host/myapp"
ssh_host = "bastion"
"#;
        tmp.write_all(content.as_bytes()).unwrap();
        let config = GlobalConfig::load_from(tmp.path()).unwrap();
        let minimal = config.connection.get("minimal").unwrap();
        assert_eq!(minimal.ssh_host.as_deref(), Some("bastion"));
        assert!(minimal.ssh_user.is_none());
        assert!(minimal.ssh_port.is_none());
        assert!(minimal.ssh_key.is_none());
    }

    #[test]
    fn ssh_host_and_key_get_env_interpolation() {
        std::env::set_var("FERRULE_TEST_BASTION", "bastion.prod");
        std::env::set_var("FERRULE_TEST_KEYDIR", "/keys");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let content = r#"
[connection.tmpl]
url = "postgres://app@db/myapp"
ssh_host = "${FERRULE_TEST_BASTION}"
ssh_key  = "${FERRULE_TEST_KEYDIR}/id_rsa"
"#;
        tmp.write_all(content.as_bytes()).unwrap();
        let config = GlobalConfig::load_from(tmp.path()).unwrap();
        let tmpl = config.connection.get("tmpl").unwrap();
        assert_eq!(tmpl.ssh_host.as_deref(), Some("bastion.prod"));
        assert_eq!(tmpl.ssh_key.as_deref(), Some("/keys/id_rsa"));
        std::env::remove_var("FERRULE_TEST_BASTION");
        std::env::remove_var("FERRULE_TEST_KEYDIR");
    }
}
