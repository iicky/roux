pub mod godoc;
pub mod griffe;
pub mod html;
pub mod markdown;
pub mod openapi;
pub mod rustdoc;
pub mod treesitter;
pub mod typescript;

use std::path::Path;

use anyhow::Result;

use crate::source::Source;

/// Maximum file size to read (10 MB).
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Maximum directory walk depth to prevent infinite recursion.
const MAX_WALK_DEPTH: usize = 100;

/// Read a file with size limit. Returns None (with warning) if too large or unreadable.
pub fn safe_read_file(path: &Path) -> Option<String> {
    match std::fs::metadata(path) {
        Ok(meta) => {
            if meta.len() > MAX_FILE_SIZE {
                eprintln!(
                    "warning: skipping oversized file {} ({:.1} MB > {:.0} MB limit)",
                    path.display(),
                    meta.len() as f64 / 1_048_576.0,
                    MAX_FILE_SIZE as f64 / 1_048_576.0
                );
                return None;
            }
        }
        Err(e) => {
            eprintln!("warning: skipping unreadable file {}: {e}", path.display());
            return None;
        }
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("warning: skipping unreadable file {}: {e}", path.display());
            None
        }
    }
}

/// Check if a path is a symlink.
pub fn is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct RawChunk {
    pub source_name: String,
    pub source_version: String,
    pub language: String,
    pub item_type: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub doc: String,
    pub body: String,
    pub url: Option<String>,
}

impl RawChunk {
    /// Build the body text used for embedding.
    pub fn build_body(
        item_type: &str,
        qualified_name: &str,
        signature: Option<&str>,
        doc: &str,
    ) -> String {
        let mut body = format!("{item_type}: {qualified_name}");
        if let Some(sig) = signature {
            body.push('\n');
            body.push_str(sig);
        }
        body.push('\n');
        body.push_str(doc);
        body
    }

    /// Compute the chunk ID as a blake3 hash of source + qualified name.
    pub fn id(&self) -> String {
        let input = format!("{}:{}", self.source_name, self.qualified_name);
        blake3::hash(input.as_bytes()).to_hex().to_string()
    }
}

pub trait Extractor {
    fn can_handle(&self, source: &Source) -> bool;
    fn extract(&self, source: &Source) -> Result<Vec<RawChunk>>;
}

/// Returns all extractors in priority order (most specific first, treesitter fallback last).
pub fn registry() -> Vec<Box<dyn Extractor>> {
    vec![
        Box::new(rustdoc::RustdocExtractor),
        Box::new(markdown::MarkdownExtractor),
        Box::new(html::HtmlExtractor),
        Box::new(openapi::OpenApiExtractor),
        Box::new(griffe::GriffeExtractor),
        Box::new(typescript::TypeScriptExtractor),
        Box::new(godoc::GoDocExtractor),
        Box::new(treesitter::TreeSitterExtractor), // fallback, last
    ]
}

/// Find the first matching extractor and extract chunks from the source.
pub fn extract(source: &Source) -> Result<Vec<RawChunk>> {
    let extractors = registry();
    for ext in &extractors {
        if ext.can_handle(source) {
            return ext.extract(source);
        }
    }
    anyhow::bail!("no extractor found for source: {}", source.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_chunk_body() {
        let body = RawChunk::build_body(
            "function",
            "tokio::spawn",
            Some("pub fn spawn<F>(future: F)"),
            "Spawns a new async task.",
        );
        assert!(body.starts_with("function: tokio::spawn"));
        assert!(body.contains("pub fn spawn<F>(future: F)"));
        assert!(body.contains("Spawns a new async task."));
    }

    #[test]
    fn test_raw_chunk_id_deterministic() {
        let chunk = RawChunk {
            source_name: "tokio".to_string(),
            source_version: "1.0.0".to_string(),
            language: "rust".to_string(),
            item_type: "function".to_string(),
            qualified_name: "tokio::spawn".to_string(),
            signature: None,
            doc: String::new(),
            body: String::new(),
            url: None,
        };

        let id1 = chunk.id();
        let id2 = chunk.id();
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 64); // blake3 hex
    }

    #[test]
    fn test_registry_not_empty() {
        let r = registry();
        assert_eq!(r.len(), 8);
    }

    #[test]
    fn test_extract_crate_routes_to_rustdoc() {
        let source = Source::from_raw("tokio", None, None, None);
        let extractors = registry();
        let matched = extractors.iter().find(|e| e.can_handle(&source));
        assert!(matched.is_some(), "crate source should match an extractor");
    }

    #[test]
    fn test_is_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("real.txt");
        std::fs::write(&file, "hello").unwrap();
        assert!(!is_symlink(&file));

        #[cfg(unix)]
        {
            let link = dir.path().join("link.txt");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(is_symlink(&link));
        }
    }

    #[test]
    fn test_safe_read_file_normal() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "content").unwrap();
        assert_eq!(safe_read_file(&file), Some("content".to_string()));
    }

    #[test]
    fn test_safe_read_file_missing() {
        let result = safe_read_file(std::path::Path::new("/nonexistent/file.txt"));
        assert!(result.is_none());
    }
}
