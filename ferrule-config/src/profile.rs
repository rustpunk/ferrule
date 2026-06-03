use crate::error::ConfigError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Global configuration + per-connection profiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default)]
    pub default: DefaultProfile,
    #[serde(default)]
    pub connection: IndexMap<String, ConnectionProfile>,
    #[serde(default)]
    pub history: HistoryConfig,
    #[serde(default)]
    pub slow_log: SlowLogConfig,
    #[serde(default)]
    pub cache: CacheConfig,
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
#[serde(deny_unknown_fields)]
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

/// Configuration for the persistent query-history store at
/// `~/.local/share/ferrule/history.db` (R4 / #4).
///
/// Default-on. The `FERRULE_NO_HISTORY` env var kills recording for a
/// single invocation without touching this config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryConfig {
    #[serde(default = "default_history_enabled")]
    pub enabled: bool,
    /// Open-loop retention: rows older than this many days are pruned
    /// opportunistically on the next `record()` call. `0` disables age-based
    /// pruning.
    #[serde(default = "default_history_max_age_days")]
    pub max_age_days: u32,
    /// Open-loop retention: cap on the total row count. `0` disables
    /// count-based pruning. Pruning, when triggered, deletes the oldest
    /// rows first.
    #[serde(default = "default_history_max_rows")]
    pub max_rows: u64,
    /// Path to the SQLite store. When `None`, defaults to
    /// `<XDG_DATA_HOME>/ferrule/history.db` (Linux/macOS) or the platform
    /// equivalent via `dirs::data_local_dir()`.
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_history_enabled(),
            max_age_days: default_history_max_age_days(),
            max_rows: default_history_max_rows(),
            path: None,
        }
    }
}

fn default_history_enabled() -> bool {
    true
}

fn default_history_max_age_days() -> u32 {
    30
}

fn default_history_max_rows() -> u64 {
    100_000
}

/// Slow-query log configuration (P5 / #16). Default off — recording
/// every slow query to a side file is opinionated enough to warrant an
/// explicit opt-in. When enabled, the `HistoryDb::record` hook also
/// appends a tab-separated line to `path` for every run whose
/// `duration_ms >= threshold_ms`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlowLogConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Threshold expressed as `humantime` (e.g. `"1s"`, `"250ms"`,
    /// `"2m"`). The CLI also accepts a bare integer of milliseconds.
    #[serde(default = "default_slow_threshold")]
    pub threshold: String,
    /// Append target. When `None`, defaults to
    /// `<XDG_DATA_HOME>/ferrule/slow.log`.
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for SlowLogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_slow_threshold(),
            path: None,
        }
    }
}

impl SlowLogConfig {
    /// Resolve `threshold` to a millisecond count. Accepts a bare integer
    /// (`"500"` → 500ms) or any humantime-style duration string. Returns
    /// `Err` with a clear message on bad input.
    pub fn threshold_ms(&self) -> Result<u64, String> {
        parse_threshold_ms(&self.threshold)
    }
}

fn default_slow_threshold() -> String {
    "1s".to_string()
}

/// Result-cache configuration (R5 / #5). Default-on with a conservative
/// 5-minute TTL. Backed by a *separate* `results.db` (NOT the history
/// store — cache eviction churns faster than telemetry retention).
///
/// The `FERRULE_NO_CACHE` env var kills the cache for a single
/// invocation without touching this config. Passing `--cache 0` to
/// `ferrule query` also bypasses the cache for that one run.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    #[serde(default = "default_cache_enabled")]
    pub enabled: bool,
    /// Default TTL applied to inserts when the caller didn't pass an
    /// explicit `--cache DURATION`. Same `s`/`m`/`h`/`d` suffix grammar
    /// as `--since`. `"0"` (the literal string) disables insertion
    /// without disabling lookup of existing entries.
    #[serde(default = "default_cache_ttl")]
    pub default_ttl: String,
    /// Open-loop retention: rows older than this many days are pruned
    /// opportunistically on the next `prune()` pass. `0` disables.
    #[serde(default = "default_cache_max_age_days")]
    pub max_age_days: u32,
    /// Open-loop retention: cap on total rows. Oldest rows are
    /// discarded first. `0` disables.
    #[serde(default = "default_cache_max_rows")]
    pub max_rows: u64,
    /// Path to the SQLite store. When `None`, defaults to
    /// `<XDG_DATA_HOME>/ferrule/results.db` via `dirs::data_local_dir()`.
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: default_cache_enabled(),
            default_ttl: default_cache_ttl(),
            max_age_days: default_cache_max_age_days(),
            max_rows: default_cache_max_rows(),
            path: None,
        }
    }
}

fn default_cache_enabled() -> bool {
    true
}

fn default_cache_ttl() -> String {
    "5m".to_string()
}

fn default_cache_max_age_days() -> u32 {
    7
}

fn default_cache_max_rows() -> u64 {
    10_000
}

/// Resolve a `[slow_log] threshold` string to milliseconds.
///
/// A bare integer is the slow-log-specific shorthand for milliseconds
/// (`"500"` → 500 ms); that quirk is handled here. Everything with a unit
/// suffix delegates to the shared [`crate::parse::parse_duration`] so the
/// recognised units stay in lock-step with `ferrule history --since`.
fn parse_threshold_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("threshold is empty".into());
    }
    // Slow-log-specific quirk: a bare integer means milliseconds.
    if let Ok(ms) = s.parse::<u64>() {
        return Ok(ms);
    }
    crate::parse::parse_duration(s)
        .map(|d| d.num_milliseconds() as u64)
        .map_err(|e| format!("threshold: {e}"))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

    fn slow(t: &str) -> SlowLogConfig {
        SlowLogConfig {
            enabled: true,
            threshold: t.into(),
            path: None,
        }
    }

    #[test]
    fn slow_log_threshold_parses_humantime_and_bare_ms() {
        assert_eq!(SlowLogConfig::default().threshold_ms().unwrap(), 1_000);
        assert_eq!(slow("250ms").threshold_ms().unwrap(), 250);
        assert_eq!(slow("500").threshold_ms().unwrap(), 500);
        assert_eq!(slow("2s").threshold_ms().unwrap(), 2_000);
        assert_eq!(slow("5m").threshold_ms().unwrap(), 300_000);
        assert_eq!(slow("1h").threshold_ms().unwrap(), 3_600_000);
    }

    #[test]
    fn slow_log_threshold_rejects_bad_input() {
        assert!(slow("").threshold_ms().is_err());
        assert!(slow("fast").threshold_ms().is_err());
        assert!(slow("5x").threshold_ms().is_err());
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
        assert_eq!(
            tunneled.ssh_key.as_deref(),
            Some("/home/me/.ssh/id_ed25519")
        );
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
