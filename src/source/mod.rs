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
