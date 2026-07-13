use std::io::Read;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// Hard cap on a downloaded `.crate` payload (compressed). Well above any real
/// crate; guards against a pathological/malicious registry response reading an
/// unbounded body into memory.
const MAX_CRATE_BYTES: u64 = 100 * 1024 * 1024;

/// Hard cap on total DECOMPRESSED bytes written during extraction. Guards
/// against a tarbomb: a tiny download that expands to fill the disk.
const MAX_EXTRACTED_BYTES: u64 = 500 * 1024 * 1024;

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
/// Returns (extracted_dir_path, resolved_version).
pub fn download_crate(name: &str, version: &str) -> Result<(PathBuf, String)> {
    validate_crate_name(name)?;
    let resolved_version;
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
        resolved_version = ver.to_string();
        format!("https://crates.io/api/v1/crates/{name}/{ver}/download")
    } else {
        resolved_version = version.to_string();
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

    let declared_len = response.content_length();
    let bytes = download_body_capped(response, declared_len, MAX_CRATE_BYTES)
        .with_context(|| format!("downloading crate {name}"))?;

    let tmp_dir = tempfile::tempdir().context("creating temp directory")?;

    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    extract_tar_safely(decoder, tmp_dir.path(), MAX_EXTRACTED_BYTES)?;

    let tmp_path = tmp_dir.keep();
    let entries: Vec<_> = std::fs::read_dir(&tmp_path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    if let Some(entry) = entries.first() {
        Ok((entry.path(), resolved_version))
    } else {
        Ok((tmp_path, resolved_version))
    }
}

/// Collect at most `max` bytes from a download body, failing if the stream is —
/// or claims via `declared_len` to be — larger. Streams through `take(max + 1)`
/// so a chunked or length-less response is capped too, not just one with a header.
fn download_body_capped<R: Read>(body: R, declared_len: Option<u64>, max: u64) -> Result<Vec<u8>> {
    if let Some(len) = declared_len
        && len > max
    {
        anyhow::bail!("download too large: {len} bytes exceeds the {max}-byte cap");
    }
    let mut buf = Vec::new();
    body.take(max + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > max {
        anyhow::bail!("download exceeds the {max}-byte cap");
    }
    Ok(buf)
}

/// Extract a tar archive into `dest_root`, rejecting entries that could escape
/// it: `..`/absolute paths (classic path traversal), and symlink/hardlink
/// entries — an earlier link entry could point outside the root for a later
/// entry to write through. A running decompressed-size cap rejects tarbombs.
fn extract_tar_safely<R: Read>(
    reader: R,
    dest_root: &std::path::Path,
    max_extracted: u64,
) -> Result<()> {
    let canonical_root = dest_root.canonicalize()?;
    let mut archive = tar::Archive::new(reader);
    let mut extracted: u64 = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        let path = entry.path()?.into_owned();
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
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            anyhow::bail!("refusing to extract link tar entry: {}", path.display());
        }
        extracted = extracted.saturating_add(entry.size());
        if extracted > max_extracted {
            anyhow::bail!(
                "refusing to extract: archive exceeds the {max_extracted}-byte extracted-size cap"
            );
        }
        let dest = canonical_root.join(&path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use tar::{EntryType, Header};

    use super::*;

    fn append_normal_file(builder: &mut tar::Builder<Vec<u8>>, path: &str, data: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, path, data).unwrap();
    }

    /// Writes `name` straight into the header's raw name field, bypassing
    /// `Header::set_path` (used by `append_data`/`append_link`), which
    /// rejects any `..` component outright. This is the only way to build a
    /// path-traversal entry to exercise the unsafe-path check.
    fn tar_with_raw_name(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(EntryType::Regular);
        header.as_mut_bytes()[..name.len()].copy_from_slice(name);
        header.set_cksum();
        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, data).unwrap();
        builder.into_inner().unwrap()
    }

    #[test]
    fn download_body_capped_returns_bytes_within_cap() {
        let bytes = download_body_capped(Cursor::new(vec![1u8; 10]), Some(10), 100).unwrap();
        assert_eq!(bytes.len(), 10);
    }

    #[test]
    fn download_body_capped_rejects_oversized_content_length() {
        let err = download_body_capped(Cursor::new(vec![0u8; 1]), Some(1000), 100).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn download_body_capped_rejects_oversized_stream_without_length() {
        let err = download_body_capped(Cursor::new(vec![0u8; 200]), None, 100).unwrap_err();
        assert!(err.to_string().contains("exceeds"));

        // Understated (not just absent) declared length must not bypass the
        // actual-bytes-read check.
        let err = download_body_capped(Cursor::new(vec![0u8; 200]), Some(5), 100).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn extract_tar_safely_unpacks_normal_file() {
        let mut builder = tar::Builder::new(Vec::new());
        append_normal_file(&mut builder, "pkg/foo.rs", b"fn main() {}");
        let tar_bytes = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        extract_tar_safely(Cursor::new(tar_bytes), dir.path(), 1 << 20).unwrap();

        let contents = std::fs::read(dir.path().join("pkg/foo.rs")).unwrap();
        assert_eq!(contents, b"fn main() {}");
    }

    #[test]
    fn extract_tar_safely_rejects_symlink() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "pkg/evil", "/etc/passwd")
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err = extract_tar_safely(Cursor::new(tar_bytes), dir.path(), 1 << 20).unwrap_err();
        assert!(err.to_string().contains("link"));
    }

    #[test]
    fn extract_tar_safely_rejects_hardlink() {
        let dir = tempfile::tempdir().unwrap();
        // Link to a target that genuinely exists on disk, so that if the
        // link-type check were ever removed the entry would actually
        // extract successfully (proving this test's failure is pinned on
        // the check, not on an incidental "no such file" from `unpack`).
        let real_file = dir.path().join("real.txt");
        std::fs::write(&real_file, b"real").unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Link);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "pkg/evil", &real_file)
            .unwrap();
        let tar_bytes = builder.into_inner().unwrap();

        let err = extract_tar_safely(Cursor::new(tar_bytes), dir.path(), 1 << 20).unwrap_err();
        assert!(err.to_string().contains("link"));
    }

    #[test]
    fn extract_tar_safely_rejects_parent_dir_path() {
        let tar_bytes = tar_with_raw_name(b"../escape.rs", b"x");

        let dir = tempfile::tempdir().unwrap();
        let err = extract_tar_safely(Cursor::new(tar_bytes), dir.path(), 1 << 20).unwrap_err();
        assert!(err.to_string().contains("unsafe path"));
    }

    #[test]
    fn extract_tar_safely_rejects_tarbomb_over_cap() {
        let mut builder = tar::Builder::new(Vec::new());
        append_normal_file(&mut builder, "pkg/big.bin", &[0u8; 100]);
        let tar_bytes = builder.into_inner().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let err = extract_tar_safely(Cursor::new(tar_bytes), dir.path(), 10).unwrap_err();
        assert!(err.to_string().contains("extracted-size cap"));
    }
}
