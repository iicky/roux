pub mod crate_download;

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub version: Option<String>,
    pub kind: SourceKind,
    pub language: Option<String>,
}

#[derive(Debug, Clone)]
pub enum SourceKind {
    /// A crate/package name to fetch from a registry
    Crate(String),
    /// A local directory to extract from
    LocalPath(PathBuf),
    /// A URL to fetch and extract
    Url(String),
    /// A single local file
    File(PathBuf),
}

impl Source {
    /// Detect source kind from a raw string (CLI argument).
    pub fn from_raw(
        raw: &str,
        name: Option<String>,
        language: Option<String>,
        version: Option<String>,
    ) -> Self {
        let kind = if raw.starts_with("http://") || raw.starts_with("https://") {
            SourceKind::Url(raw.to_string())
        } else {
            let path = PathBuf::from(raw);
            if path.exists() {
                if path.is_dir() {
                    SourceKind::LocalPath(path)
                } else {
                    SourceKind::File(path)
                }
            } else {
                // Assume it's a crate/package name
                SourceKind::Crate(raw.to_string())
            }
        };

        let detected_name = name.unwrap_or_else(|| match &kind {
            SourceKind::Crate(name) => name.clone(),
            SourceKind::LocalPath(p) => p
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| raw.to_string()),
            SourceKind::Url(u) => u.clone(),
            SourceKind::File(p) => p
                .file_stem()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| raw.to_string()),
        });

        Self {
            name: detected_name,
            version,
            kind,
            language,
        }
    }

    /// Detect language from file extension if not explicitly set.
    pub fn detected_language(&self) -> Option<&str> {
        if let Some(ref lang) = self.language {
            return Some(lang);
        }

        match &self.kind {
            SourceKind::Crate(_) => Some("rust"),
            SourceKind::File(p) => match p.extension().and_then(|e| e.to_str()) {
                Some("rs") => Some("rust"),
                Some("py") => Some("python"),
                Some("ts" | "tsx") => Some("typescript"),
                Some("js" | "jsx") => Some("javascript"),
                Some("go") => Some("go"),
                Some("pl" | "pm") => Some("perl"),
                Some("md" | "markdown") => Some("markdown"),
                Some("html" | "htm") => Some("html"),
                Some("yaml" | "yml" | "json") => None, // could be openapi or anything
                _ => None,
            },
            _ => None,
        }
    }

    /// Detect format hint from file extension for extractor routing.
    pub fn format_hint(&self) -> Option<&str> {
        match &self.kind {
            SourceKind::Crate(_) => Some("rustdoc"),
            SourceKind::Url(_) => Some("html"),
            SourceKind::File(p) => match p.extension().and_then(|e| e.to_str()) {
                Some("md" | "markdown") => Some("markdown"),
                Some("html" | "htm") => Some("html"),
                Some("yaml" | "yml") => Some("openapi"),
                Some("json") => Some("openapi"), // could be rustdoc json too
                _ => None,
            },
            SourceKind::LocalPath(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_raw_crate_name() {
        let src = Source::from_raw("tokio", None, None, None);
        assert!(matches!(src.kind, SourceKind::Crate(ref s) if s == "tokio"));
        assert_eq!(src.name, "tokio");
    }

    #[test]
    fn test_from_raw_url() {
        let src = Source::from_raw("https://docs.rs/tokio", None, None, None);
        assert!(matches!(src.kind, SourceKind::Url(_)));
        assert_eq!(src.name, "https://docs.rs/tokio");
    }

    #[test]
    fn test_from_raw_http_url() {
        let src = Source::from_raw("http://example.com/docs", None, None, None);
        assert!(matches!(src.kind, SourceKind::Url(_)));
    }

    #[test]
    fn test_from_raw_local_file() {
        // Cargo.toml exists in the repo
        let src = Source::from_raw("Cargo.toml", None, None, None);
        assert!(matches!(src.kind, SourceKind::File(_)));
        assert_eq!(src.name, "Cargo");
    }

    #[test]
    fn test_from_raw_local_dir() {
        let src = Source::from_raw("src", None, None, None);
        assert!(matches!(src.kind, SourceKind::LocalPath(_)));
        assert_eq!(src.name, "src");
    }

    #[test]
    fn test_from_raw_with_name_override() {
        let src = Source::from_raw("tokio", Some("my-tokio".to_string()), None, None);
        assert_eq!(src.name, "my-tokio");
    }

    #[test]
    fn test_from_raw_with_language_override() {
        let src = Source::from_raw("tokio", None, Some("python".to_string()), None);
        assert_eq!(src.detected_language(), Some("python"));
    }

    #[test]
    fn test_detected_language_crate() {
        let src = Source::from_raw("tokio", None, None, None);
        assert_eq!(src.detected_language(), Some("rust"));
    }

    #[test]
    fn test_detected_language_extensions() {
        let cases = vec![
            ("foo.rs", Some("rust")),
            ("foo.py", Some("python")),
            ("foo.ts", Some("typescript")),
            ("foo.tsx", Some("typescript")),
            ("foo.js", Some("javascript")),
            ("foo.go", Some("go")),
            ("foo.md", Some("markdown")),
            ("foo.html", Some("html")),
            ("foo.yaml", None),
            ("foo.unknown", None),
        ];

        for (filename, expected) in cases {
            // These files don't exist, so they'll be treated as crate names
            // We need to construct Source directly to test File variant
            let src = Source {
                name: filename.to_string(),
                version: None,
                kind: SourceKind::File(PathBuf::from(filename)),
                language: None,
            };
            assert_eq!(src.detected_language(), expected, "failed for {filename}");
        }
    }

    #[test]
    fn test_format_hint_crate() {
        let src = Source::from_raw("tokio", None, None, None);
        assert_eq!(src.format_hint(), Some("rustdoc"));
    }

    #[test]
    fn test_format_hint_url() {
        let src = Source::from_raw("https://docs.rs/tokio", None, None, None);
        assert_eq!(src.format_hint(), Some("html"));
    }

    #[test]
    fn test_format_hint_files() {
        let cases = vec![
            ("foo.md", Some("markdown")),
            ("foo.html", Some("html")),
            ("foo.yaml", Some("openapi")),
            ("foo.json", Some("openapi")),
            ("foo.rs", None),
        ];

        for (filename, expected) in cases {
            let src = Source {
                name: filename.to_string(),
                version: None,
                kind: SourceKind::File(PathBuf::from(filename)),
                language: None,
            };
            assert_eq!(src.format_hint(), expected, "failed for {filename}");
        }
    }

    #[test]
    fn test_format_hint_local_path() {
        let src = Source {
            name: "myproject".to_string(),
            version: None,
            kind: SourceKind::LocalPath(PathBuf::from("/tmp/myproject")),
            language: None,
        };
        assert_eq!(src.format_hint(), None);
    }
}
