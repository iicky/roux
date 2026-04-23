//! Portable roux index artifacts — the format CI produces for agents to consume.
//!
//! An artifact is a standalone SQLite file (schema v6) with extra manifest rows
//! in the `metadata` table identifying the producer. Optionally gzip-wrapped.
//!
//! ## Spec v1
//!
//! Required metadata rows:
//! - `artifact_version`        — spec version, currently `"1"`
//! - `artifact_roux_version`   — semver of the producing roux binary
//! - `artifact_schema_version` — integer schema version
//! - `artifact_created_at`     — unix timestamp (seconds)
//!
//! Optional metadata rows:
//! - `artifact_commit`         — git SHA of the source repo
//! - `artifact_repo`           — repo identifier (e.g. `owner/name`)
//!
//! Compatibility rules consumers enforce via `check_artifact_compatibility`:
//! - Missing `artifact_version` ⇒ not a roux artifact
//! - `artifact_version != "1"` ⇒ refuse
//! - `artifact_schema_version` exceeds this binary's max ⇒ refuse with upgrade hint
//! - Older schema is migrated up on open by `GraphStore::open`.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::GzEncoder;
use rusqlite::Connection;

use crate::graph::store::GraphStore;

pub const ARTIFACT_VERSION: &str = "1";
pub const SUPPORTED_SCHEMA_MAX: i64 = 6;

/// Write a portable artifact at `output`. `source_db` is the sqlite file to export.
///
/// Uses `VACUUM INTO` to produce a compact copy, stamps manifest metadata, then
/// optionally gzip-wraps. If the final path exists it is overwritten.
pub fn export(source_db: &Path, output: &Path, gzip: bool) -> Result<PathBuf> {
    if !source_db.exists() {
        bail!("no index at {}", source_db.display());
    }

    // VACUUM INTO needs a path the sqlite engine can create. Work through a temp
    // path even for the uncompressed case so we can swap atomically into place.
    let tmp = tempfile::NamedTempFile::new().context("creating temp file")?;
    let tmp_path = tmp.path().to_path_buf();
    // VACUUM INTO refuses to write to an existing file. Drop the tempfile's
    // empty placeholder while retaining the unique path.
    drop(tmp);

    {
        let src = Connection::open(source_db)
            .with_context(|| format!("opening {}", source_db.display()))?;
        let quoted = tmp_path.to_string_lossy().replace('\'', "''");
        src.execute_batch(&format!("VACUUM INTO '{quoted}';"))
            .context("VACUUM INTO")?;
    }

    // Stamp manifest rows onto the vacuumed copy.
    {
        let dst = Connection::open(&tmp_path).context("opening vacuumed copy")?;
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let roux_version = env!("CARGO_PKG_VERSION");

        // schema_version already lives in metadata; we surface it under the
        // artifact_ prefix too so consumers can read the manifest in one query.
        let schema_version: String = dst
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| SUPPORTED_SCHEMA_MAX.to_string());

        let rows: &[(&str, String)] = &[
            ("artifact_version", ARTIFACT_VERSION.to_string()),
            ("artifact_roux_version", roux_version.to_string()),
            ("artifact_schema_version", schema_version),
            ("artifact_created_at", created_at.to_string()),
        ];
        for (k, v) in rows {
            dst.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
                rusqlite::params![k, v],
            )?;
        }
        // Optional: if CI exposes the commit/repo, record them.
        if let Ok(commit) = std::env::var("GITHUB_SHA") {
            dst.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('artifact_commit', ?1)",
                rusqlite::params![commit],
            )?;
        }
        if let Ok(repo) = std::env::var("GITHUB_REPOSITORY") {
            dst.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('artifact_repo', ?1)",
                rusqlite::params![repo],
            )?;
        }
    }

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok();
    }

    if gzip {
        let bytes = std::fs::read(&tmp_path).context("reading vacuumed file")?;
        let out = std::fs::File::create(output)
            .with_context(|| format!("creating {}", output.display()))?;
        let mut enc = GzEncoder::new(out, Compression::default());
        enc.write_all(&bytes)?;
        enc.finish()?;
    } else {
        std::fs::rename(&tmp_path, output)
            .or_else(|_| std::fs::copy(&tmp_path, output).map(|_| ()))
            .with_context(|| format!("writing {}", output.display()))?;
    }
    // Best-effort cleanup if rename fell through to copy.
    let _ = std::fs::remove_file(&tmp_path);

    Ok(output.to_path_buf())
}

/// Verify that `path` is a roux artifact this binary can read. Returns the
/// detected `artifact_version` on success.
pub fn check_artifact_compatibility(path: &Path) -> Result<String> {
    let conn = Connection::open(path)
        .with_context(|| format!("opening {}", path.display()))?;

    // Artifact version
    let artifact_version: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'artifact_version'",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(v) = artifact_version else {
        bail!(
            "{} is not a roux artifact (missing artifact_version). Rebuild with `roux export`.",
            path.display()
        );
    };
    if v != ARTIFACT_VERSION {
        bail!(
            "artifact at {} uses spec v{v}, this roux supports v{ARTIFACT_VERSION}. Upgrade one side.",
            path.display()
        );
    }

    // Schema version
    let schema_version: i64 = conn
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if schema_version > SUPPORTED_SCHEMA_MAX {
        bail!(
            "artifact schema v{schema_version} is newer than this roux (max v{SUPPORTED_SCHEMA_MAX}). Upgrade roux or rebuild the artifact."
        );
    }

    // Opening through GraphStore runs migrations for older schemas — probe it.
    let _ = GraphStore::open(path)
        .with_context(|| format!("opening {} as GraphStore", path.display()))?;

    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Edge, Node};
    use tempfile::TempDir;

    fn make_test_node(name: &str) -> Node {
        let qn = format!("test::{name}");
        Node {
            id: Node::id_for("test", &qn),
            kind: "function".into(),
            name: name.into(),
            qualified_name: qn,
            source_name: "test".into(),
            language: "rust".into(),
            file_path: "lib.rs".into(),
            start_line: 1,
            start_col: 0,
            end_line: 3,
            visibility: "pub".into(),
            signature: Some(format!("fn {name}()")),
            doc: None,
            body: format!("function: test::{name}"),
            parent_id: None,
            content_hash: None,
            line_count: 3,
            source_url: None,
            description: None,
        }
    }

    fn seed_store(path: &Path) {
        let store = GraphStore::open(path).unwrap();
        let nodes = vec![make_test_node("alpha"), make_test_node("beta")];
        let edges: Vec<Edge> = vec![];
        store
            .upsert_source("test", "1.0.0", "rust", &nodes, &edges)
            .unwrap();
    }

    #[test]
    fn export_roundtrip_uncompressed() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.sqlite");
        seed_store(&src);

        let out = tmp.path().join("artifact.sqlite");
        export(&src, &out, false).unwrap();

        // Artifact validates.
        let v = check_artifact_compatibility(&out).unwrap();
        assert_eq!(v, ARTIFACT_VERSION);

        // And queries return the same data.
        let store = GraphStore::open(&out).unwrap();
        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "test");
        assert_eq!(sources[0].node_count, 2);
    }

    #[test]
    fn export_roundtrip_gzipped() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.sqlite");
        seed_store(&src);

        let out = tmp.path().join("artifact.sqlite.gz");
        export(&src, &out, true).unwrap();

        // Gzipped output doesn't validate as a sqlite file.
        assert!(check_artifact_compatibility(&out).is_err());

        // Decompress and re-check.
        let gz = std::fs::File::open(&out).unwrap();
        let mut dec = flate2::read::GzDecoder::new(gz);
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut dec, &mut buf).unwrap();
        let decompressed = tmp.path().join("decompressed.sqlite");
        std::fs::write(&decompressed, &buf).unwrap();

        check_artifact_compatibility(&decompressed).unwrap();
    }

    #[test]
    fn check_rejects_non_artifact_sqlite() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("raw.sqlite");
        // Plain GraphStore with no artifact_* rows — not an artifact.
        seed_store(&path);

        let err = check_artifact_compatibility(&path).unwrap_err();
        assert!(err.to_string().contains("not a roux artifact"));
    }

    #[test]
    fn check_rejects_wrong_artifact_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("v2.sqlite");
        seed_store(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('artifact_version', '2')",
            [],
        )
        .unwrap();

        let err = check_artifact_compatibility(&path).unwrap_err();
        assert!(err.to_string().contains("spec v2"));
    }

    #[test]
    fn export_stamps_manifest_rows() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("source.sqlite");
        seed_store(&src);
        let out = tmp.path().join("artifact.sqlite");
        export(&src, &out, false).unwrap();

        let conn = Connection::open(&out).unwrap();
        let roux_version: String = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'artifact_roux_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(roux_version, env!("CARGO_PKG_VERSION"));

        let created_at: i64 = conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'artifact_created_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(created_at > 0);
    }
}
