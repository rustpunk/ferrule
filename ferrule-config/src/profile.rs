#![allow(dead_code, unused_variables, unused_imports)]

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Global configuration + per-connection profiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    pub default: DefaultProfile,
    #[serde(default)]
    pub connection: IndexMap<String, ConnectionProfile>,
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
    pub headers: IndexMap<String, String>,
}
