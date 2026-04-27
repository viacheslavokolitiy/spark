//! Application configuration, loaded from `config.yml`.

use serde::Deserialize;
use std::path::PathBuf;

/// Returns the default history file path.
fn default_history_file() -> PathBuf {
    PathBuf::from("history.jsonl")
}

/// Returns the default saved requests file path.
fn default_saved_requests_file() -> PathBuf {
    PathBuf::from("saved_requests.json")
}

/// Top-level application configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Path to the JSONL file where request history is persisted.
    #[serde(default = "default_history_file", alias = "history")]
    pub history_file: PathBuf,
    /// Path to the JSON file where saved requests are persisted.
    #[serde(default = "default_saved_requests_file")]
    pub saved_requests_file: PathBuf,
}

impl Config {
    /// Loads configuration from `config.yml` in the current working directory.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or the YAML cannot be parsed.
    pub fn load() -> anyhow::Result<Self> {
        let content = std::fs::read_to_string("config.yml")?;
        let config = serde_yaml::from_str(&content)?;
        Ok(config)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            history_file: default_history_file(),
            saved_requests_file: default_saved_requests_file(),
        }
    }
}
