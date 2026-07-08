pub mod extract;
pub mod rank;
pub mod store;
pub mod tags;

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
    /// Auto-generated natural language description from graph context.
    /// Bridges the semantic gap for undocumented symbols.
    pub description: Option<String>,
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

    /// ID for a file-scoped symbol, disambiguated by `file_path` and an
    /// optional `discriminator` (typically the parameter list, for overloads).
    ///
    /// A top-level symbol's qualified name is just `source::name` (no file
    /// path), so two private `helper`s in different files, `main` in a
    /// multi-binary crate, or a symbol redefined per file all hash to the same
    /// `id_for` and get silently merged by `merge_duplicate_nodes` — with the
    /// loser's edges misattributed to the survivor. Folding
    /// `file_path` into the hash keeps them distinct. File nodes stay on
    /// `id_for`: their qualified name already embeds the path.
    ///
    /// Same-file overloads (`add(int)` vs `add(double)`) share a signatureless
    /// qualified name and would still collide, so callers pass the parameter
    /// list as `discriminator` for function/method kinds — overloads differ in
    /// it, while a forward declaration and its definition share it and stay
    /// merged. Non-overloadable kinds pass `""`.
    pub fn id_for_symbol(
        source_name: &str,
        file_path: &str,
        qualified_name: &str,
        discriminator: &str,
    ) -> String {
        let input = format!("{source_name}:{file_path}:{qualified_name}:{discriminator}");
        blake3::hash(input.as_bytes()).to_hex().to_string()
    }

    /// Parameter-list discriminator for overload disambiguation: the
    /// whitespace-collapsed contents of the signature's first balanced
    /// parenthesis group, or `""` when there is none.
    ///
    /// Overloads differ here (`(int a, int b)` vs `(double a, double b)`); a
    /// prototype and its definition match (`(int x)` either way). Parameter
    /// *names* are kept, so a decl/def pair that renames a parameter across the
    /// header/source boundary won't merge — a rare, cosmetic duplicate.
    pub fn param_discriminator(signature: Option<&str>) -> String {
        let Some(sig) = signature else {
            return String::new();
        };
        let Some(open) = sig.find('(') else {
            return String::new();
        };
        let mut depth = 0i32;
        let mut close = None;
        for (i, b) in sig.bytes().enumerate().skip(open) {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else {
            return String::new();
        };
        sig[open + 1..close].split_whitespace().collect::<Vec<_>>().join(" ")
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
