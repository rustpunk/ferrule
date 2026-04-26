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
        // Apply env interpolation to profile URLs
        for profile in config.connection.values_mut() {
            profile.url = crate::registry::interpolate_env_vars(&profile.url);
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
}
