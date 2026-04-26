use crate::error::ConfigError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Lightweight entry for a single connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionEntry {
    pub name: String,
    pub url: String,
}

/// In-memory registry of connections.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectionRegistry {
    pub entries: IndexMap<String, ConnectionEntry>,
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: String, url: String) -> Result<(), ConfigError> {
        if self.entries.contains_key(&name) {
            return Err(ConfigError::DuplicateConnection(name));
        }
        self.entries
            .insert(name.clone(), ConnectionEntry { name, url });
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<(), ConfigError> {
        self.entries
            .shift_remove(name)
            .ok_or_else(|| ConfigError::ConnectionNotFound(name.to_string()))?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ConnectionEntry> {
        self.entries.get(name)
    }

    pub fn list(&self) -> Vec<&ConnectionEntry> {
        self.entries.values().collect()
    }

    /// Load from the default config directory (`~/.config/ferrule/connections.toml`).
    pub fn load_default() -> Result<Self, ConfigError> {
        let path = default_config_path()?;
        if !path.exists() {
            return Ok(Self::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut registry: ConnectionRegistry =
            toml::from_str(&content).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        for entry in registry.entries.values_mut() {
            entry.url = interpolate_env_vars(&entry.url);
        }
        Ok(registry)
    }

    /// Save to the default config directory.
    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = default_config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content =
            toml::to_string(self).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}

fn default_config_path() -> Result<std::path::PathBuf, ConfigError> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| ConfigError::ConfigNotFound("could not determine config directory".into()))?
        .join("ferrule");
    Ok(config_dir.join("connections.toml"))
}

/// Interpolate `${VAR}` and `${VAR:-default}` patterns in a string.
/// Also supports `$$` as an escaped literal `$`.
/// Unknown variables are left unchanged.
pub fn interpolate_env_vars(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.next_if_eq(&'$').is_some() {
                out.push('$');
                continue;
            }
            if chars.next_if_eq(&'{').is_some() {
                let var_spec: String = chars.by_ref().take_while(|c| *c != '}').collect();
                if let Some((var, default)) = var_spec.split_once(":-") {
                    match std::env::var(var) {
                        Ok(val) if !val.is_empty() => out.push_str(&val),
                        _ => out.push_str(default),
                    }
                } else {
                    match std::env::var(&var_spec) {
                        Ok(val) => out.push_str(&val),
                        Err(_) => {
                            out.push_str("${");
                            out.push_str(&var_spec);
                            out.push('}');
                        }
                    }
                }
            } else {
                out.push('$');
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interpolate_basic() {
        std::env::set_var("FERRULE_TEST_DB", "mydb");
        assert_eq!(
            interpolate_env_vars("postgres://u@h/${FERRULE_TEST_DB}"),
            "postgres://u@h/mydb"
        );
    }

    #[test]
    fn test_interpolate_default() {
        std::env::remove_var("FERRULE_TEST_MISSING");
        assert_eq!(
            interpolate_env_vars("host=${FERRULE_TEST_MISSING:-localhost}"),
            "host=localhost"
        );
    }

    #[test]
    fn test_interpolate_default_override() {
        std::env::set_var("FERRULE_TEST_HOST", "prod.example.com");
        assert_eq!(
            interpolate_env_vars("host=${FERRULE_TEST_HOST:-localhost}"),
            "host=prod.example.com"
        );
        std::env::remove_var("FERRULE_TEST_HOST");
    }

    #[test]
    fn test_interpolate_escape() {
        assert_eq!(interpolate_env_vars("cost is $$5.00"), "cost is $5.00");
    }

    #[test]
    fn test_interpolate_unknown() {
        std::env::remove_var("FERRULE_TEST_UNKNOWN");
        assert_eq!(
            interpolate_env_vars("host=${FERRULE_TEST_UNKNOWN}"),
            "host=${FERRULE_TEST_UNKNOWN}"
        );
    }

    #[test]
    fn test_interpolate_no_braces() {
        // Bare $VAR is left as-is (not interpolated)
        assert_eq!(interpolate_env_vars("host=$VAR"), "host=$VAR");
    }
}
