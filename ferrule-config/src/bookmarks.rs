use crate::error::ConfigError;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A single saved query bookmark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bookmark {
    pub sql: String,
    pub connection: Option<String>,
}

/// Persistent store for bookmarks using TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BookmarkStore {
    #[serde(flatten)]
    pub bookmarks: IndexMap<String, Bookmark>,
}

impl BookmarkStore {
    /// Return the default file path (`~/.config/ferrule/bookmarks.toml`).
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| {
                ConfigError::ConfigNotFound("could not determine config directory".into())
            })?
            .join("ferrule");
        Ok(config_dir.join("bookmarks.toml"))
    }

    /// Load from the default path.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from_path(&Self::default_path()?)
    }

    /// Load from an explicit path.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        let store: BookmarkStore =
            toml::from_str(&content).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        Ok(store)
    }

    /// Save to the default path.
    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to_path(&Self::default_path()?)
    }

    /// Save to an explicit path.
    pub fn save_to_path(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(ConfigError::Io)?;
        }
        let content =
            toml::to_string(self).map_err(|e| ConfigError::InvalidConfig(e.to_string()))?;
        std::fs::write(path, content).map_err(ConfigError::Io)?;
        Ok(())
    }

    /// Look up a bookmark by its full dotted name.
    pub fn get(&self, name: &str) -> Option<&Bookmark> {
        self.bookmarks.get(name)
    }

    /// Insert or overwrite a bookmark.
    pub fn insert(&mut self, name: String, sql: String, connection: Option<String>) {
        self.bookmarks.insert(name, Bookmark { sql, connection });
    }

    /// Remove a bookmark by name.
    pub fn remove(&mut self, name: &str) -> Result<(), ConfigError> {
        self.bookmarks
            .shift_remove(name)
            .ok_or_else(|| ConfigError::ConnectionNotFound(name.to_string()))?;
        Ok(())
    }

    /// Return an ordered list of all bookmarks.
    pub fn list(&self) -> Vec<(&String, &Bookmark)> {
        self.bookmarks.iter().collect()
    }

    /// Extract the connection hint from a dotted bookmark name.
    ///
    /// * `pg.select_users` → `Some("pg")`
    /// * `count_all`       → `None`
    pub fn connection_hint(name: &str) -> Option<&str> {
        if let Some((prefix, _rest)) = name.split_once('.') {
            Some(prefix)
        } else {
            None
        }
    }

    /// Perform positional substitution of `${1}`, `${2}`, etc.
    ///
    /// Missing parameters leave the placeholder intact.
    pub fn resolve_params(sql: &str, params: &[String]) -> String {
        let mut result = sql.to_string();
        for (i, param) in params.iter().enumerate() {
            let placeholder = format!("${{{}}}", i + 1);
            result = result.replace(&placeholder, param);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bookmark_param_substitution() {
        let sql = "SELECT * FROM users WHERE id = ${1} AND name = ${2}";
        let result = BookmarkStore::resolve_params(sql, &["42".into(), "Alice".into()]);
        assert_eq!(result, "SELECT * FROM users WHERE id = 42 AND name = Alice");
    }

    #[test]
    fn test_bookmark_missing_param_left_intact() {
        let sql = "SELECT * FROM users WHERE id = ${1} AND x = ${3}";
        let result = BookmarkStore::resolve_params(sql, &["42".into()]);
        assert_eq!(result, "SELECT * FROM users WHERE id = 42 AND x = ${3}");
    }

    #[test]
    fn test_connection_hint() {
        assert_eq!(
            BookmarkStore::connection_hint("pg.select_users"),
            Some("pg")
        );
        assert_eq!(BookmarkStore::connection_hint("count_all"), None);
        assert_eq!(
            BookmarkStore::connection_hint("prod.db.users"),
            Some("prod")
        );
    }

    #[test]
    fn test_roundtrip() {
        let mut store = BookmarkStore::default();
        store.insert(
            "pg.select_users".into(),
            "SELECT * FROM users;".into(),
            Some("pg".into()),
        );
        store.insert(
            "count_all".into(),
            "SELECT COUNT(*) FROM ${1};".into(),
            None,
        );
        assert_eq!(store.bookmarks.len(), 2);

        let (name, bm) = store.list()[0];
        assert_eq!(name, "pg.select_users");
        assert_eq!(bm.sql, "SELECT * FROM users;");
        assert_eq!(bm.connection.as_deref(), Some("pg"));

        store.remove("pg.select_users").unwrap();
        assert_eq!(store.bookmarks.len(), 1);
        assert!(store.get("pg.select_users").is_none());
    }
}
