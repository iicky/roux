use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub search: SearchConfig,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexConfig {
    #[serde(default = "default_global_path")]
    pub global_path: PathBuf,
    #[serde(default = "default_true")]
    pub prefer_local: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_top_k")]
    pub default_top_k: usize,
}

fn home_dir_fallback() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn default_global_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| home_dir_fallback().join(".local").join("share"))
        .join("roux")
        .join("db.sqlite")
}

fn default_true() -> bool {
    true
}

fn default_top_k() -> usize {
    5
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            global_path: default_global_path(),
            prefer_local: true,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_top_k: default_top_k(),
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
            .unwrap_or_else(|| home_dir_fallback().join(".config"))
            .join("roux")
            .join("config.toml")
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(toml::from_str(s)?)
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(config.index.prefer_local);
        assert_eq!(config.search.default_top_k, 5);
    }

    #[test]
    fn test_empty_toml_gives_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.search.default_top_k, 5);
    }

    #[test]
    fn test_partial_toml_override() {
        let config: Config = toml::from_str(
            r#"
            [search]
            default_top_k = 10
            "#,
        )
        .unwrap();
        assert_eq!(config.search.default_top_k, 10);
        assert!(config.index.prefer_local); // still default
    }

    #[test]
    fn test_resolve_store_path_local() {
        let config = Config::default();
        let path = config.resolve_store_path(true);
        assert_eq!(path, PathBuf::from(".roux/db.sqlite"));
    }

    #[test]
    fn test_from_str() {
        let config = Config::parse("[search]\ndefault_top_k = 20").unwrap();
        assert_eq!(config.search.default_top_k, 20);
    }
}
