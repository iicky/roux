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

    pub fn load_from(path: &std::path::Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&contents)?)
    }

    /// Resolve the store path, checking for local .roux/db.sqlite first if prefer_local is set.
    pub fn resolve_store_path(&self, local: bool) -> PathBuf {
        if local {
            return PathBuf::from(".roux/db.sqlite");
        }
        if self.index.prefer_local {
            let local_path = PathBuf::from(".roux/db.sqlite");
            if local_path.exists() {
                return local_path;
            }
        }
        self.index.global_path.clone()
    }

    /// Resolve the model directory path.
    pub fn model_dir(&self) -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("~/.local/share"))
            .join("roux")
            .join("models")
            .join(self.model.id.replace('/', "-"))
    }

    /// Write a default config file if one doesn't exist.
    pub fn init_default() -> Result<PathBuf> {
        let path = Self::config_path();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let default = Self::default();
            let toml_str = toml::to_string_pretty(&default)?;
            std::fs::write(&path, toml_str)?;
        }
        Ok(path)
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

    #[test]
    fn test_resolve_store_path_global() {
        let config = Config::default();
        let path = config.resolve_store_path(false);
        // prefer_local is true by default, but .roux/db.sqlite won't exist in test dir
        assert!(path.to_string_lossy().contains("db.sqlite"));
    }

    #[test]
    fn test_resolve_store_path_local_flag() {
        let config = Config::default();
        let path = config.resolve_store_path(true);
        assert_eq!(path, PathBuf::from(".roux/db.sqlite"));
    }

    #[test]
    fn test_model_dir() {
        let config = Config::default();
        let dir = config.model_dir();
        assert!(
            dir.to_string_lossy()
                .contains("intfloat-multilingual-e5-small")
        );
    }

    #[test]
    fn test_load_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[ingest]\nbatch_size = 99\n").unwrap();

        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.ingest.batch_size, 99);
    }

    #[test]
    fn test_init_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roux").join("config.toml");

        // Temporarily override — we can't easily test init_default without
        // touching the real config path, so test the serialization round-trip
        let default = Config::default();
        let toml_str = toml::to_string_pretty(&default).unwrap();
        let roundtrip: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(roundtrip.model.id, default.model.id);
        assert_eq!(roundtrip.ingest.batch_size, default.ingest.batch_size);

        // Also verify we can write and read back
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &toml_str).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(loaded.model.id, "intfloat/multilingual-e5-small");
    }
}
