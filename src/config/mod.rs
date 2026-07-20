use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub index: IndexConfig,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexConfig {
    #[serde(default = "default_global_path")]
    pub global_path: PathBuf,
    #[serde(default = "default_true")]
    pub prefer_local: bool,
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

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            global_path: default_global_path(),
            prefer_local: true,
        }
    }
}

impl Config {
    /// Load the roux configuration from disk, applying the `ROUX_GLOBAL_PATH`
    /// environment override over the config file.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        let mut config = if path.exists() {
            let contents = std::fs::read_to_string(&path)?;
            toml::from_str(&contents)?
        } else {
            Self::default()
        };
        // `ROUX_GLOBAL_PATH` overrides the global store location on every OS,
        // taking precedence over the config file. It redirects the index for
        // unusual setups and lets tests isolate the global store cross-platform
        // (the `dirs` data dir cannot be redirected by env on Windows).
        if let Some(p) = std::env::var_os("ROUX_GLOBAL_PATH") {
            config.index.global_path = PathBuf::from(p);
        }
        Ok(config)
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

    pub fn resolve_store_path(&self, scope: StoreScope) -> PathBuf {
        match scope {
            StoreScope::Local => PathBuf::from(".roux").join("db.sqlite"),
            StoreScope::Global => self.index.global_path.clone(),
            StoreScope::Auto => {
                if self.index.prefer_local {
                    let local_path = PathBuf::from(".roux").join("db.sqlite");
                    if local_path.exists() {
                        return local_path;
                    }
                }
                self.index.global_path.clone()
            }
        }
    }
}

/// Which store a CLI command should target. `Auto` follows `prefer_local`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreScope {
    /// Force `.roux/db.sqlite`.
    Local,
    /// Force the configured global store even if a local index exists.
    Global,
    /// Default behavior: prefer local if `prefer_local` is true and local exists.
    Auto,
}

impl StoreScope {
    /// Translate mutually-exclusive CLI flags into a scope. Callers should use
    /// clap's `conflicts_with` so both flags can't be set at once.
    pub fn from_flags(local: bool, global: bool) -> Self {
        match (local, global) {
            (true, false) => StoreScope::Local,
            (false, true) => StoreScope::Global,
            _ => StoreScope::Auto,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(config.index.prefer_local);
    }

    #[test]
    fn test_empty_toml_gives_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.index.prefer_local);
    }

    #[test]
    fn test_partial_toml_override() {
        let config: Config = toml::from_str(
            r#"
            [index]
            prefer_local = false
            "#,
        )
        .unwrap();
        assert!(!config.index.prefer_local);
    }

    #[test]
    fn test_resolve_store_path_local() {
        let config = Config::default();
        let path = config.resolve_store_path(StoreScope::Local);
        assert_eq!(path, PathBuf::from(".roux").join("db.sqlite"));
    }

    #[test]
    fn test_resolve_store_path_global_overrides_prefer_local() {
        let mut config = Config::default();
        config.index.prefer_local = true;
        // Even with prefer_local and a potentially-present .roux, Global forces global.
        let path = config.resolve_store_path(StoreScope::Global);
        assert_eq!(path, config.index.global_path);
    }

    #[test]
    fn test_scope_from_flags() {
        assert_eq!(StoreScope::from_flags(true, false), StoreScope::Local);
        assert_eq!(StoreScope::from_flags(false, true), StoreScope::Global);
        assert_eq!(StoreScope::from_flags(false, false), StoreScope::Auto);
    }

    #[test]
    fn test_from_str() {
        let config = Config::parse("[index]\nprefer_local = false").unwrap();
        assert!(!config.index.prefer_local);
    }
}
