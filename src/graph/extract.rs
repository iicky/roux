use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Language, Node, Parser};

use super::{Edge, Symbol};

/// Extraction result from a single file.
pub struct FileGraph {
    pub symbols: Vec<Symbol>,
    pub edges: Vec<Edge>,
}

/// Extract symbols and edges from a source directory.
pub fn extract_dir(
    dir: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
) -> Result<FileGraph> {
    let mut all_symbols = Vec::new();
    let mut all_edges = Vec::new();

    walk_dir(
        dir,
        dir,
        source_name,
        source_version,
        language_hint,
        &mut all_symbols,
        &mut all_edges,
        0,
    )?;

    // Build cross-file reference edges
    resolve_references(&mut all_edges, &all_symbols);

    Ok(FileGraph {
        symbols: all_symbols,
        edges: all_edges,
    })
}

/// Extract symbols and edges from a single file.
pub fn extract_file(
    path: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
) -> Result<FileGraph> {
    let lang = language_hint
        .or_else(|| detect_language(path))
        .context("cannot detect language")?;

    let ts_lang = get_ts_language(lang).with_context(|| format!("unsupported language: {lang}"))?;

    let code =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let rel_path = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut symbols = Vec::new();
    let mut edges = Vec::new();

    extract_from_source(
        &code,
        ts_lang,
        lang,
        &rel_path,
        source_name,
        source_version,
        &mut symbols,
        &mut edges,
    )?;

    Ok(FileGraph { symbols, edges })
}

fn walk_dir(
    dir: &Path,
    base: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<Edge>,
    depth: usize,
) -> Result<()> {
    if depth > 100 {
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        // Skip symlinks, hidden dirs, node_modules, target, __pycache__
        if path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "__pycache__"
                || name == "vendor"
                || name == ".git"
            {
                continue;
            }
        }

        if path.is_dir() {
            walk_dir(
                &path,
                base,
                source_name,
                source_version,
                language_hint,
                symbols,
                edges,
                depth + 1,
            )?;
            continue;
        }

        let lang = language_hint.or_else(|| detect_language(&path));
        let lang = match lang {
            Some(l) => l,
            None => continue,
        };

        let ts_lang = match get_ts_language(lang) {
            Some(l) => l,
            None => continue,
        };

        let code = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Check file size (10MB limit)
        if code.len() > 10 * 1024 * 1024 {
            continue;
        }

        let rel_path = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let _ = extract_from_source(
            &code,
            ts_lang,
            lang,
            &rel_path,
            source_name,
            source_version,
            symbols,
            edges,
        );
    }

    Ok(())
}

fn detect_language(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("py") => Some("python"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("js" | "jsx" | "mjs") => Some("javascript"),
        Some("go") => Some("go"),
        _ => None,
    }
}

fn get_ts_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" | "tsx" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        _ => None,
    }
}

/// Core extraction: parse source code and emit symbols + edges.
fn extract_from_source(
    code: &str,
    ts_lang: Language,
    lang: &str,
    file_path: &str,
    source_name: &str,
    source_version: &str,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<Edge>,
) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .context("setting parser language")?;

    let tree = parser.parse(code, None).context("parsing source code")?;

    let root = tree.root_node();
    let code_bytes = code.as_bytes();

    extract_node(
        &root,
        code_bytes,
        lang,
        file_path,
        source_name,
        source_version,
        symbols,
        edges,
        None, // no parent
        "",   // no prefix
    );

    Ok(())
}

/// Recursively extract symbols from a tree-sitter node.
fn extract_node(
    node: &Node,
    code: &[u8],
    lang: &str,
    file_path: &str,
    source_name: &str,
    source_version: &str,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<Edge>,
    parent_id: Option<&str>,
    prefix: &str,
) {
    let kind = node.kind();

    // Try to extract a symbol from this node
    if let Some(mut sym) = match lang {
        "rust" => extract_rust_symbol(node, code, kind),
        "python" => extract_python_symbol(node, code, kind),
        "javascript" | "typescript" | "tsx" => extract_js_symbol(node, code, kind),
        "go" => extract_go_symbol(node, code, kind),
        _ => None,
    } {
        // Build qualified name
        let qualified = if prefix.is_empty() {
            format!("{source_name}::{}", sym.name)
        } else {
            format!("{prefix}::{}", sym.name)
        };
        sym.qualified_name = qualified.clone();
        sym.source_name = source_name.to_string();
        sym.source_version = source_version.to_string();
        sym.language = lang.to_string();
        sym.file_path = file_path.to_string();
        sym.line = node.start_position().row + 1;
        sym.id = Symbol::id_for(source_name, &sym.qualified_name);
        sym.parent_id = parent_id.map(|s| s.to_string());
        sym.body = sym.build_body();

        let sym_id = sym.id.clone();
        let sym_kind = sym.kind.clone();

        // Add "contains" edge from parent
        if let Some(pid) = parent_id {
            edges.push(Edge {
                from_id: pid.to_string(),
                to_id: sym_id.clone(),
                kind: "contains".to_string(),
            });
        }

        symbols.push(sym);

        // Recurse into children with this symbol as parent
        let new_prefix = qualified;
        let child_parent = if matches!(
            sym_kind.as_str(),
            "class" | "module" | "struct" | "enum" | "trait" | "impl"
        ) {
            Some(sym_id.as_str())
        } else {
            parent_id
        };

        // For function bodies, extract identifier references as potential call edges
        if matches!(sym_kind.as_str(), "function" | "method") {
            extract_call_references(node, code, &sym_id, edges);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            extract_node(
                &child,
                code,
                lang,
                file_path,
                source_name,
                source_version,
                symbols,
                edges,
                child_parent,
                &new_prefix,
            );
        }
    } else {
        // Not a symbol node — recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            extract_node(
                &child,
                code,
                lang,
                file_path,
                source_name,
                source_version,
                symbols,
                edges,
                parent_id,
                prefix,
            );
        }
    }
}

/// Extract identifier references from a function body as potential "calls" edges.
fn extract_call_references(node: &Node, code: &[u8], caller_id: &str, edges: &mut Vec<Edge>) {
    let mut cursor = node.walk();
    extract_calls_recursive(node, &mut cursor, code, caller_id, edges);
}

fn extract_calls_recursive(
    node: &Node,
    cursor: &mut tree_sitter::TreeCursor,
    code: &[u8],
    caller_id: &str,
    edges: &mut Vec<Edge>,
) {
    // Look for call expressions
    if node.kind() == "call_expression"
        || node.kind() == "call"
        || node.kind() == "macro_invocation"
    {
        // The function being called is usually the first child
        if let Some(func_node) = node
            .child_by_field_name("function")
            .or_else(|| node.child_by_field_name("name"))
            .or_else(|| node.child(0))
        {
            let callee_name = node_text(&func_node, code).to_string();
            if !callee_name.is_empty() && callee_name.len() < 200 {
                // Store as an unresolved reference — we'll resolve to actual symbol IDs later
                edges.push(Edge {
                    from_id: caller_id.to_string(),
                    to_id: format!("__unresolved::{callee_name}"),
                    kind: "calls".to_string(),
                });
            }
        }
        return; // Don't recurse into call children
    }

    let mut child_cursor = node.walk();
    for child in node.children(&mut child_cursor) {
        extract_calls_recursive(&child, cursor, code, caller_id, edges);
    }
}

/// Resolve unresolved reference edges to actual symbol IDs.
fn resolve_references(edges: &mut Vec<Edge>, symbols: &[Symbol]) {
    for edge in edges.iter_mut() {
        if let Some(ref_name) = edge.to_id.strip_prefix("__unresolved::") {
            // Try to find a matching symbol by name
            if let Some(target) = symbols.iter().find(|s| {
                s.name == ref_name || s.qualified_name.ends_with(&format!("::{ref_name}"))
            }) {
                edge.to_id = target.id.clone();
            }
        }
    }

    // Remove edges that couldn't be resolved (external calls, stdlib, etc.)
    edges.retain(|e| !e.to_id.starts_with("__unresolved::"));
}

// ─── Language-specific symbol extraction ───────────────────────────────

fn node_text<'a>(node: &Node, code: &'a [u8]) -> &'a str {
    node.utf8_text(code).unwrap_or("")
}

fn find_child_by_kind<'a>(node: &Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

fn extract_doc_comment(node: &Node, code: &[u8]) -> Option<String> {
    // Look for comment siblings immediately before this node
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();

    while let Some(sib) = sibling {
        match sib.kind() {
            "line_comment" | "comment" => {
                let text = node_text(&sib, code);
                let cleaned = text
                    .trim_start_matches("///")
                    .trim_start_matches("//!")
                    .trim_start_matches("//")
                    .trim_start_matches('#')
                    .trim();
                comments.push(cleaned.to_string());
                sibling = sib.prev_sibling();
            }
            "block_comment" | "doc_comment" => {
                let text = node_text(&sib, code);
                let cleaned = clean_block_comment(text);
                if !cleaned.is_empty() {
                    comments.push(cleaned);
                }
                sibling = sib.prev_sibling();
            }
            _ => break,
        }
    }

    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    Some(comments.join("\n"))
}

fn extract_python_docstring(node: &Node, code: &[u8]) -> Option<String> {
    // Python docstrings are the first expression_statement in a function/class body
    let body = find_child_by_kind(node, "block")?;
    let mut cursor = body.walk();
    let first_stmt = body
        .children(&mut cursor)
        .find(|c| c.kind() == "expression_statement")?;
    let string_node = first_stmt.child(0)?;

    if string_node.kind() == "string" || string_node.kind() == "concatenated_string" {
        let text = node_text(&string_node, code);
        let cleaned = text
            .trim_start_matches("\"\"\"")
            .trim_end_matches("\"\"\"")
            .trim_start_matches("'''")
            .trim_end_matches("'''")
            .trim();
        if !cleaned.is_empty() {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn clean_block_comment(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.trim()
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches("* ")
                .trim_start_matches('*')
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn extract_rust_symbol(node: &Node, code: &[u8], kind: &str) -> Option<Symbol> {
    match kind {
        "function_item" | "function_signature_item" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(&name, "function", sig, doc))
        }
        "struct_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(
                &name,
                "struct",
                Some(format!("struct {}", &name)),
                doc,
            ))
        }
        "enum_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(
                &name,
                "enum",
                Some(format!("enum {}", &name)),
                doc,
            ))
        }
        "trait_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(
                &name,
                "trait",
                Some(format!("trait {}", &name)),
                doc,
            ))
        }
        "impl_item" => {
            let type_node = find_child_by_kind(node, "type_identifier")
                .or_else(|| find_child_by_kind(node, "generic_type"))?;
            let name = node_text(&type_node, code).to_string();
            Some(make_symbol(&name, "impl", None, None))
        }
        "mod_item" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(&name, "module", None, doc))
        }
        "const_item" | "static_item" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(
                &name,
                "const",
                Some(format!("const {name}")),
                doc,
            ))
        }
        "type_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(
                &name,
                "type",
                Some(format!("type {name}")),
                doc,
            ))
        }
        _ => None,
    }
}

fn extract_python_symbol(node: &Node, code: &[u8], kind: &str) -> Option<Symbol> {
    match kind {
        "function_definition" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            if name.starts_with('_') && !name.starts_with("__") {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc =
                extract_python_docstring(node, code).or_else(|| extract_doc_comment(node, code));
            Some(make_symbol(&name, "function", sig, doc))
        }
        "class_definition" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let doc =
                extract_python_docstring(node, code).or_else(|| extract_doc_comment(node, code));
            Some(make_symbol(
                &name,
                "class",
                Some(format!("class {name}")),
                doc,
            ))
        }
        _ => None,
    }
}

fn extract_js_symbol(node: &Node, code: &[u8], kind: &str) -> Option<Symbol> {
    match kind {
        "function_declaration" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(&name, "function", sig, doc))
        }
        "class_declaration" => {
            let name = node_text(
                &find_child_by_kind(node, "identifier")
                    .or_else(|| find_child_by_kind(node, "type_identifier"))?,
                code,
            )
            .to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(
                &name,
                "class",
                Some(format!("class {name}")),
                doc,
            ))
        }
        "export_statement" => {
            // Check if it's exporting a declaration
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if let Some(sym) = extract_js_symbol(&child, code, child.kind()) {
                    return Some(sym);
                }
            }
            None
        }
        "method_definition" => {
            let name =
                node_text(&find_child_by_kind(node, "property_identifier")?, code).to_string();
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(&name, "method", sig, doc))
        }
        "lexical_declaration" => {
            // export const foo = (...) => ...
            let decl = find_child_by_kind(node, "variable_declarator")?;
            let name = node_text(&find_child_by_kind(&decl, "identifier")?, code).to_string();
            let value = decl.child_by_field_name("value")?;
            if value.kind() == "arrow_function" || value.kind() == "function" {
                let doc = extract_doc_comment(node, code);
                Some(make_symbol(&name, "function", None, doc))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_go_symbol(node: &Node, code: &[u8], kind: &str) -> Option<Symbol> {
    match kind {
        "function_declaration" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            // Only export uppercase functions in Go
            if !name.starts_with(|c: char| c.is_uppercase()) {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(&name, "function", sig, doc))
        }
        "method_declaration" => {
            let name = node_text(&find_child_by_kind(node, "field_identifier")?, code).to_string();
            if !name.starts_with(|c: char| c.is_uppercase()) {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_symbol(&name, "method", sig, doc))
        }
        "type_declaration" => {
            let spec = find_child_by_kind(node, "type_spec")?;
            let name = node_text(&find_child_by_kind(&spec, "type_identifier")?, code).to_string();
            if !name.starts_with(|c: char| c.is_uppercase()) {
                return None;
            }
            let doc = extract_doc_comment(node, code);
            let type_kind = if find_child_by_kind(&spec, "struct_type").is_some() {
                "struct"
            } else if find_child_by_kind(&spec, "interface_type").is_some() {
                "interface"
            } else {
                "type"
            };
            Some(make_symbol(&name, type_kind, None, doc))
        }
        _ => None,
    }
}

fn make_symbol(name: &str, kind: &str, signature: Option<String>, doc: Option<String>) -> Symbol {
    let name = name.to_string();
    Symbol {
        id: String::new(), // filled in by extract_node
        kind: kind.to_string(),
        name,
        qualified_name: String::new(), // filled in by extract_node
        source_name: String::new(),
        source_version: String::new(),
        language: String::new(),
        file_path: String::new(),
        line: 0,
        signature,
        doc,
        body: String::new(),
        parent_id: None,
    }
}

/// Extract the signature line(s) from a node — everything up to the body.
fn extract_signature_text(node: &Node, code: &[u8]) -> Option<String> {
    let start = node.start_byte();
    // Find the body block (first { or : in the node)
    let body_node = find_child_by_kind(node, "block")
        .or_else(|| find_child_by_kind(node, "declaration_list"))
        .or_else(|| find_child_by_kind(node, "field_declaration_list"));

    let end = body_node
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte().min(start + 500));

    let sig = std::str::from_utf8(&code[start..end]).ok()?.trim();
    if sig.is_empty() {
        None
    } else {
        Some(sig.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract_rust(code: &str) -> FileGraph {
        let mut symbols = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            "test.rs",
            "test",
            "1.0.0",
            &mut symbols,
            &mut edges,
        )
        .unwrap();
        resolve_references(&mut edges, &symbols);
        FileGraph { symbols, edges }
    }

    fn extract_python(code: &str) -> FileGraph {
        let mut symbols = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_python::LANGUAGE.into(),
            "python",
            "test.py",
            "test",
            "1.0.0",
            &mut symbols,
            &mut edges,
        )
        .unwrap();
        resolve_references(&mut edges, &symbols);
        FileGraph { symbols, edges }
    }

    fn extract_js(code: &str) -> FileGraph {
        let mut symbols = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            "test.js",
            "test",
            "1.0.0",
            &mut symbols,
            &mut edges,
        )
        .unwrap();
        resolve_references(&mut edges, &symbols);
        FileGraph { symbols, edges }
    }

    #[test]
    fn test_rust_function() {
        let g = extract_rust("pub fn spawn() {}");
        assert_eq!(g.symbols.len(), 1);
        assert_eq!(g.symbols[0].name, "spawn");
        assert_eq!(g.symbols[0].kind, "function");
    }

    #[test]
    fn test_rust_struct_and_impl() {
        let g = extract_rust(
            r#"
            pub struct Foo {}
            impl Foo {
                pub fn new() -> Self { Foo {} }
            }
            "#,
        );
        let names: Vec<&str> = g.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"new"));
    }

    #[test]
    fn test_rust_trait() {
        let g = extract_rust(
            r#"
            pub trait Serializable {
                fn serialize(&self) -> String;
            }
            "#,
        );
        let kinds: Vec<&str> = g.symbols.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains(&"trait"));
    }

    #[test]
    fn test_rust_doc_comment() {
        let g = extract_rust(
            r#"
            /// Spawns a new task.
            pub fn spawn() {}
            "#,
        );
        assert_eq!(g.symbols.len(), 1);
        assert!(
            g.symbols[0]
                .doc
                .as_deref()
                .unwrap()
                .contains("Spawns a new task"),
            "doc: {:?}",
            g.symbols[0].doc
        );
    }

    #[test]
    fn test_rust_call_edges() {
        let g = extract_rust(
            r#"
            pub fn helper() {}
            pub fn main_fn() {
                helper();
            }
            "#,
        );
        let call_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "calls").collect();
        assert!(
            !call_edges.is_empty(),
            "should have call edges: {:?}",
            g.edges
        );
    }

    #[test]
    fn test_python_function() {
        let g = extract_python(
            r#"
def greet(name):
    """Say hello."""
    print(f"hello {name}")
            "#,
        );
        assert_eq!(g.symbols.len(), 1);
        assert_eq!(g.symbols[0].name, "greet");
        assert!(g.symbols[0].doc.as_deref().unwrap().contains("Say hello"));
    }

    #[test]
    fn test_python_class() {
        let g = extract_python(
            r#"
class MyModel:
    """A model."""

    def predict(self, x):
        """Run prediction."""
        return x
            "#,
        );
        let names: Vec<&str> = g.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"MyModel"));
        assert!(names.contains(&"predict"));
    }

    #[test]
    fn test_js_function() {
        let g = extract_js(
            r#"
            function greet(name) {
                return "hello " + name;
            }
            "#,
        );
        assert_eq!(g.symbols.len(), 1);
        assert_eq!(g.symbols[0].name, "greet");
    }

    #[test]
    fn test_js_class() {
        let g = extract_js(
            r#"
            class App {
                render() {
                    return null;
                }
            }
            "#,
        );
        let names: Vec<&str> = g.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"App"));
        assert!(names.contains(&"render"));
    }

    #[test]
    fn test_parent_child_relationship() {
        let g = extract_rust(
            r#"
            pub mod auth {
                pub fn login() {}
            }
            "#,
        );
        let login = g.symbols.iter().find(|s| s.name == "login").unwrap();
        assert!(login.parent_id.is_some());

        let contains_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "contains").collect();
        assert!(!contains_edges.is_empty());
    }

    #[test]
    fn test_qualified_names() {
        let g = extract_rust(
            r#"
            pub mod auth {
                pub fn login() {}
            }
            "#,
        );
        let login = g.symbols.iter().find(|s| s.name == "login").unwrap();
        assert!(
            login.qualified_name.contains("auth::login"),
            "got: {}",
            login.qualified_name
        );
    }

    #[test]
    fn test_extract_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn hello() {}\npub fn world() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        assert!(g.symbols.len() >= 2);
    }
}
