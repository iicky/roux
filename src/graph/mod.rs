pub mod extract;
pub mod rank;
pub mod store;

/// A node in the code graph — a file, function, class, etc.
#[derive(Debug, Clone)]
pub struct Node {
    /// Unique ID (blake3 hash of source_name + qualified_name)
    pub id: String,
    /// Node kind: file, module, function, method, class, struct, enum, trait, interface, const, type
    pub kind: String,
    /// Short name (e.g. "spawn", "lib.rs")
    pub name: String,
    /// Fully qualified name (e.g. "tokio::task::spawn")
    pub qualified_name: String,
    /// Source this node belongs to
    pub source_name: String,
    /// Language
    pub language: String,
    /// File path relative to source root
    pub file_path: String,
    /// Start line (1-based, 0 for file nodes)
    pub start_line: usize,
    /// Start column (0-based)
    pub start_col: usize,
    /// End line (0 = unknown)
    pub end_line: usize,
    /// Visibility: "pub", "export", "private", or ""
    pub visibility: String,
    /// Function/method signature
    pub signature: Option<String>,
    /// Documentation string
    pub doc: Option<String>,
    /// Body text for FTS indexing
    pub body: String,
    /// Parent node ID (file for top-level, class for methods, etc.)
    pub parent_id: Option<String>,
    /// Blake3 hash of the symbol's source text (for staleness detection)
    pub content_hash: Option<String>,
    /// Number of lines this symbol spans
    pub line_count: usize,
    /// URL to view this symbol online (GitHub, docs.rs, etc.)
    pub source_url: Option<String>,
}

/// An edge between two nodes (cross-references only, not containment).
#[derive(Debug, Clone)]
pub struct Edge {
    pub from_id: String,
    pub to_id: String,
    /// Relationship kind: calls, imports, implements, inherits, type_ref
    pub kind: String,
}

impl Node {
    pub fn id_for(source_name: &str, qualified_name: &str) -> String {
        let input = format!("{source_name}:{qualified_name}");
        blake3::hash(input.as_bytes()).to_hex().to_string()
    }

    /// Build the FTS body text from node metadata.
    pub fn build_body(&self) -> String {
        let mut body = format!("{}: {}", self.kind, self.qualified_name);
        if let Some(ref sig) = self.signature {
            body.push('\n');
            body.push_str(sig);
        }
        if let Some(ref doc) = self.doc {
            body.push('\n');
            body.push_str(doc);
        }
        body
    }
}
