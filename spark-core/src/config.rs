//! Application configuration, loaded from `config.yml`.

use serde::Deserialize;
use std::path::PathBuf;

/// Top-level application configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// Path to the JSONL file where request history is persisted.
    pub history_file: PathBuf,
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
            history_file: PathBuf::from("history.jsonl"),
        }
    }
}
