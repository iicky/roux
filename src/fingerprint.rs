//! Cheap content fingerprints used for staleness detection.
//!
//! Walks a directory or hashes a file to produce a deterministic value that
//! changes when the indexed content changes. We use mtime+size rather than full
//! content hashing to keep `roux list` fast — stat is O(files) with no reads.

use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

const MAX_DEPTH: usize = 100;

/// Skip rules that mirror `graph::extract::walk_dir` so the fingerprint covers
/// exactly the set of files we would re-extract.
fn skip_dir(name: &str) -> bool {
    name.starts_with('.')
        || matches!(
            name,
            "node_modules" | "target" | "__pycache__" | "vendor" | ".git"
        )
}

/// Artifact files the indexer never consumes. Keeping these out of the rollup
/// means `roux export` into the indexed directory doesn't spuriously flip the
/// local source to "stale".
fn skip_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite-journal")
        || lower.ends_with(".sqlite-wal")
        || lower.ends_with(".sqlite-shm")
        || lower.ends_with(".sqlite.gz")
        || lower.ends_with(".db")
        || lower == ".ds_store"
}

/// Rollup fingerprint of a directory: blake3 over sorted `(rel_path, size, mtime)` tuples.
pub fn fingerprint_dir(dir: &Path) -> Result<String> {
    let mut entries = Vec::new();
    collect(dir, dir, &mut entries, 0).with_context(|| format!("walking {}", dir.display()))?;
    entries.sort();

    let mut hasher = blake3::Hasher::new();
    for (path, size, mtime) in &entries {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&size.to_le_bytes());
        hasher.update(&mtime.to_le_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Fingerprint of a single file: blake3 of its contents.
pub fn fingerprint_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn collect(dir: &Path, base: &Path, out: &mut Vec<(String, u64, u64)>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && skip_dir(name)
        {
            continue;
        }

        if path.is_dir() {
            collect(&path, base, out, depth + 1)?;
        } else {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && skip_file(name)
            {
                continue;
            }
            let md = match path.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push((rel, md.len(), mtime));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn dir_fingerprint_is_deterministic() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        fs::write(tmp.path().join("b.rs"), "fn b() {}").unwrap();

        let fp1 = fingerprint_dir(tmp.path()).unwrap();
        let fp2 = fingerprint_dir(tmp.path()).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn dir_fingerprint_changes_on_edit() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("a.rs");
        fs::write(&file, "fn a() {}").unwrap();
        let fp1 = fingerprint_dir(tmp.path()).unwrap();

        // Ensure a different mtime and size
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(&file, "fn a() { let _ = 1; }").unwrap();
        let fp2 = fingerprint_dir(tmp.path()).unwrap();

        assert_ne!(fp1, fp2);
    }

    #[test]
    fn dir_fingerprint_skips_ignored_dirs() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        let fp_before = fingerprint_dir(tmp.path()).unwrap();

        // Creating node_modules/, target/, .git/ must not affect the fingerprint.
        for ignored in ["node_modules", "target", ".git", "__pycache__", "vendor"] {
            let sub = tmp.path().join(ignored);
            fs::create_dir(&sub).unwrap();
            fs::write(sub.join("junk"), "x").unwrap();
        }
        // And hidden dot-files/dirs at top level.
        fs::create_dir(tmp.path().join(".hidden")).unwrap();
        fs::write(tmp.path().join(".hidden").join("x"), "y").unwrap();

        let fp_after = fingerprint_dir(tmp.path()).unwrap();
        assert_eq!(fp_before, fp_after);
    }

    #[test]
    fn dir_fingerprint_skips_sqlite_artifacts() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        let fp_before = fingerprint_dir(tmp.path()).unwrap();

        // Exporting a roux artifact into the indexed directory must not change
        // the fingerprint — otherwise a `roux export` spuriously flips the
        // local source to stale.
        fs::write(tmp.path().join("artifact.sqlite"), "fake db bytes").unwrap();
        fs::write(tmp.path().join("artifact.sqlite.gz"), "fake gz").unwrap();
        fs::write(tmp.path().join(".DS_Store"), "macos junk").unwrap();

        let fp_after = fingerprint_dir(tmp.path()).unwrap();
        assert_eq!(fp_before, fp_after);
    }

    #[test]
    fn file_fingerprint_tracks_content() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("x.txt");
        fs::write(&p, "hello").unwrap();
        let fp1 = fingerprint_file(&p).unwrap();
        fs::write(&p, "world").unwrap();
        let fp2 = fingerprint_file(&p).unwrap();
        assert_ne!(fp1, fp2);
    }
}
