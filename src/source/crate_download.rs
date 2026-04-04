use std::path::PathBuf;

use anyhow::{Context, Result};

fn validate_crate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("invalid crate name: {name:?} (must match [a-zA-Z0-9_-]+)");
    }
    Ok(())
}

/// Download a crate from crates.io and extract to a temp directory.
pub fn download_crate(name: &str, version: &str) -> Result<PathBuf> {
    validate_crate_name(name)?;
    let url = if version == "latest" {
        let meta_url = format!("https://crates.io/api/v1/crates/{name}");
        let client = reqwest::blocking::Client::builder()
            .user_agent("roux-cli/0.0.1")
            .build()?;
        let meta: serde_json::Value = client.get(&meta_url).send()?.json()?;
        let ver = meta["crate"]["max_stable_version"]
            .as_str()
            .or_else(|| meta["crate"]["max_version"].as_str())
            .context("could not determine latest version")?;
        format!("https://crates.io/api/v1/crates/{name}/{ver}/download")
    } else {
        format!("https://crates.io/api/v1/crates/{name}/{version}/download")
    };

    eprintln!("Downloading {name}...");
    let client = reqwest::blocking::Client::builder()
        .user_agent("roux-cli/0.0.1")
        .build()?;
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("downloading crate {name}"))?;

    if !response.status().is_success() {
        anyhow::bail!("failed to download {name}: HTTP {}", response.status());
    }

    let bytes = response.bytes()?;

    let tmp_dir = tempfile::tempdir().context("creating temp directory")?;

    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);

    // Validate each entry to prevent path traversal attacks
    let canonical_tmp = tmp_dir.path().canonicalize()?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
            || path.is_absolute()
        {
            anyhow::bail!(
                "refusing to extract tar entry with unsafe path: {}",
                path.display()
            );
        }
        let dest = canonical_tmp.join(&path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&dest)?;
    }

    let tmp_path = tmp_dir.keep();
    let entries: Vec<_> = std::fs::read_dir(&tmp_path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    if let Some(entry) = entries.first() {
        Ok(entry.path())
    } else {
        Ok(tmp_path)
    }
}
