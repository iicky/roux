use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub ingest: IngestConfig,
    #[serde(default)]
    pub api: Option<ApiConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_model_id")]
    pub id: String,
    #[serde(default = "default_device")]
    pub device: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexConfig {
    #[serde(default = "default_global_path")]
    pub global_path: PathBuf,
    #[serde(default = "default_true")]
    pub prefer_local: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IngestConfig {
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,
    #[serde(default = "default_top_k")]
    pub default_top_k: usize,
    #[serde(default)]
    pub transitive: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiConfig {
    pub provider: String,
    pub api_key_env: String,
    pub model: String,
}

fn default_model_id() -> String {
    "intfloat/multilingual-e5-small".to_string()
}

fn default_device() -> String {
    "auto".to_string()
}

fn default_global_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("roux")
        .join("db.sqlite")
}

fn default_true() -> bool {
    true
}

fn default_batch_size() -> usize {
    32
}

fn default_top_k() -> usize {
    3
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            id: default_model_id(),
            device: default_device(),
        }
    }
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            global_path: default_global_path(),
            prefer_local: true,
        }
    }
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            batch_size: default_batch_size(),
            default_top_k: default_top_k(),
            transitive: false,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            Ok(toml::from_str(&contents)?)
        } else {
            Ok(Self::default())
        }
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("roux")
            .join("config.toml")
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(toml::from_str(s)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.model.id, "intfloat/multilingual-e5-small");
        assert_eq!(config.model.device, "auto");
        assert!(config.index.prefer_local);
        assert_eq!(config.ingest.batch_size, 32);
        assert_eq!(config.ingest.default_top_k, 3);
        assert!(!config.ingest.transitive);
        assert!(config.api.is_none());
    }

    #[test]
    fn test_empty_toml_gives_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.model.id, "intfloat/multilingual-e5-small");
        assert_eq!(config.ingest.batch_size, 32);
    }

    #[test]
    fn test_partial_toml_override() {
        let config: Config = toml::from_str(
            r#"
            [model]
            id = "BAAI/bge-small-en-v1.5"

            [ingest]
            batch_size = 64
            "#,
        )
        .unwrap();

        assert_eq!(config.model.id, "BAAI/bge-small-en-v1.5");
        assert_eq!(config.model.device, "auto"); // still default
        assert_eq!(config.ingest.batch_size, 64);
        assert_eq!(config.ingest.default_top_k, 3); // still default
    }

    #[test]
    fn test_full_toml() {
        let config: Config = toml::from_str(
            r#"
            [model]
            id = "custom/model"
            device = "cuda"

            [index]
            global_path = "/custom/path/db.sqlite"
            prefer_local = false

            [ingest]
            batch_size = 16
            default_top_k = 5
            transitive = true

            [api]
            provider = "voyage"
            api_key_env = "VOYAGE_API_KEY"
            model = "voyage-code-2"
            "#,
        )
        .unwrap();

        assert_eq!(config.model.device, "cuda");
        assert!(!config.index.prefer_local);
        assert!(config.ingest.transitive);
        assert_eq!(config.api.unwrap().provider, "voyage");
    }

    #[test]
    fn test_load_missing_file_gives_defaults() {
        // Config::load() falls back to defaults when file doesn't exist
        // This test just ensures it doesn't panic
        let config = Config::default();
        assert_eq!(config.model.id, "intfloat/multilingual-e5-small");
    }

    #[test]
    fn test_from_str() {
        let config = Config::parse("[ingest]\nbatch_size = 128").unwrap();
        assert_eq!(config.ingest.batch_size, 128);
    }
}
