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

    pub fn add(
        &mut self,
        name: String,
        url: String,
    ) -> Result<(), ConfigError> {
        if self.entries.contains_key(&name) {
            return Err(ConfigError::DuplicateConnection(name));
        }
        self.entries.insert(
            name.clone(),
            ConnectionEntry { name, url },
        );
        Ok(())
    }

    pub fn remove(
        &mut self,
        name: &str,
    ) -> Result<(), ConfigError> {
        self.entries
            .shift_remove(name)
            .ok_or_else(|| ConfigError::ConnectionNotFound(name.to_string()))?;
        Ok(())
    }

    pub fn get(
        &self,
        name: &str,
    ) -> Option<&ConnectionEntry> {
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
        let registry: ConnectionRegistry = toml::from_str(&content)
            .map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        Ok(registry)
    }

    /// Save to the default config directory.
    pub fn save_default(&self) -> Result<(), ConfigError> {
        let path = default_config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string(self)
            .map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
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
