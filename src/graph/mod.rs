pub mod extract;
pub mod store;

/// A symbol node in the code graph.
#[derive(Debug, Clone)]
pub struct Symbol {
    /// Unique ID (blake3 hash of source + qualified_name)
    pub id: String,
    /// Symbol kind: function, class, method, trait, struct, enum, module, const, type
    pub kind: String,
    /// Short name (e.g. "spawn")
    pub name: String,
    /// Fully qualified name (e.g. "tokio::task::spawn")
    pub qualified_name: String,
    /// Source this symbol belongs to
    pub source_name: String,
    pub source_version: String,
    /// Language
    pub language: String,
    /// File path relative to source root
    pub file_path: String,
    /// Line number in the file (1-based)
    pub line: usize,
    /// Function/method signature
    pub signature: Option<String>,
    /// Documentation string
    pub doc: Option<String>,
    /// Body text for FTS indexing (kind + qualified_name + signature + doc)
    pub body: String,
    /// Parent symbol ID (e.g. method's class, function's module)
    pub parent_id: Option<String>,
}

/// An edge between two symbols.
#[derive(Debug, Clone)]
pub struct Edge {
    /// Source symbol ID
    pub from_id: String,
    /// Target symbol ID
    pub to_id: String,
    /// Relationship kind: calls, imports, implements, contains, inherits
    pub kind: String,
}

impl Symbol {
    pub fn id_for(source_name: &str, qualified_name: &str) -> String {
        let input = format!("{source_name}:{qualified_name}");
        blake3::hash(input.as_bytes()).to_hex().to_string()
    }

    /// Build the FTS body text from symbol metadata.
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

    /// Build a compact descriptor for lightweight embedding.
    /// e.g. "fn authenticate | mod auth | calls: validate_token, hash_password"
    pub fn descriptor(&self, edges: &[Edge], symbols: &[Symbol]) -> String {
        let mut desc = format!("{} {}", self.kind, self.name);

        // Add parent context
        if let Some(ref parent_id) = self.parent_id {
            if let Some(parent) = symbols.iter().find(|s| s.id == *parent_id) {
                desc.push_str(&format!(" | {} {}", parent.kind, parent.name));
            }
        }

        // Add outgoing relationships
        let calls: Vec<&str> = edges
            .iter()
            .filter(|e| e.from_id == self.id && e.kind == "calls")
            .filter_map(|e| symbols.iter().find(|s| s.id == e.to_id))
            .map(|s| s.name.as_str())
            .collect();
        if !calls.is_empty() {
            desc.push_str(&format!(" | calls: {}", calls.join(", ")));
        }

        desc
    }
}
