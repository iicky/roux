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
}
