use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Language, Node as TsNode, Parser};

use super::{Edge, Node};

/// Extraction result from a single file.
pub struct FileGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

/// Extract nodes and edges from a source directory.
pub fn extract_dir(
    dir: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
) -> Result<FileGraph> {
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();

    walk_dir(
        dir,
        dir,
        source_name,
        source_version,
        language_hint,
        &mut all_nodes,
        &mut all_edges,
        0,
    )?;

    // Build cross-file reference edges
    resolve_references(&mut all_edges, &all_nodes);

    // Post-processing passes for convention-based edges
    let pre = all_edges.len();
    infer_test_edges(&all_nodes, &mut all_edges);
    let post_tests = all_edges.len() - pre;
    infer_override_edges(&all_nodes, &mut all_edges);
    let post_overrides = all_edges.len() - pre - post_tests;
    infer_export_edges(&all_nodes, &mut all_edges);
    let post_exports = all_edges.len() - pre - post_tests - post_overrides;
    if post_tests + post_overrides + post_exports > 0 {
        eprintln!(
            "  inferred: {post_tests} test, {post_overrides} override, {post_exports} export edges"
        );
    }

    // Generate natural language descriptions from graph context
    generate_descriptions(&mut all_nodes, &all_edges);

    Ok(FileGraph {
        nodes: all_nodes,
        edges: all_edges,
    })
}

/// Extract nodes and edges from a single file.
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

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // Create file node
    let file_qualified = format!("{source_name}::{rel_path}");
    let file_id = Node::id_for(source_name, &file_qualified);
    let file_hash = blake3::hash(code.as_bytes()).to_hex().to_string();
    let file_lines = code.lines().count();
    nodes.push(make_file_node(
        &file_id,
        &rel_path,
        &file_qualified,
        source_name,
        lang,
        &rel_path,
        Some(&file_hash),
        file_lines,
    ));

    extract_from_source(
        &code,
        ts_lang,
        lang,
        &rel_path,
        source_name,
        source_version,
        &mut nodes,
        &mut edges,
        Some(&file_id),
    )?;

    Ok(FileGraph { nodes, edges })
}

fn walk_dir(
    dir: &Path,
    base: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
    nodes: &mut Vec<Node>,
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

        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (name.starts_with('.')
                || name == "node_modules"
                || name == "target"
                || name == "__pycache__"
                || name == "vendor"
                || name == ".git")
        {
            continue;
        }

        if path.is_dir() {
            walk_dir(
                &path,
                base,
                source_name,
                source_version,
                language_hint,
                nodes,
                edges,
                depth + 1,
            )?;
            continue;
        }

        // Check for markdown docs
        let ext = path.extension().and_then(|e| e.to_str());
        if matches!(ext, Some("md" | "markdown")) {
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if content.len() > 10 * 1024 * 1024 {
                continue;
            }
            let rel_path = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            extract_markdown_doc(&content, &rel_path, source_name, nodes, edges);
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

        if code.len() > 10 * 1024 * 1024 {
            continue;
        }

        let rel_path = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        // Create a file node
        let file_qualified = format!("{source_name}::{rel_path}");
        let file_id = Node::id_for(source_name, &file_qualified);
        let file_hash = blake3::hash(code.as_bytes()).to_hex().to_string();
        let file_lines = code.lines().count();
        let file_name = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();

        nodes.push(make_file_node(
            &file_id,
            &file_name,
            &file_qualified,
            source_name,
            lang,
            &rel_path,
            Some(&file_hash),
            file_lines,
        ));

        let _ = extract_from_source(
            &code,
            ts_lang,
            lang,
            &rel_path,
            source_name,
            source_version,
            nodes,
            edges,
            Some(&file_id),
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

/// Core extraction: parse source code and emit nodes + edges.
fn extract_from_source(
    code: &str,
    ts_lang: Language,
    lang: &str,
    file_path: &str,
    source_name: &str,
    source_version: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    file_parent_id: Option<&str>,
) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .context("setting parser language")?;

    let tree = parser.parse(code, None).context("parsing source code")?;

    let root = tree.root_node();
    let code_bytes = code.as_bytes();

    // Extract import edges from top-level
    extract_imports(&root, code_bytes, lang, source_name, edges);

    extract_node(
        &root,
        code_bytes,
        lang,
        file_path,
        source_name,
        source_version,
        nodes,
        edges,
        file_parent_id,
        "",
    );

    Ok(())
}

/// Extract implements/inherits edges from class/impl/struct declarations.
fn extract_relationship_edges(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    let kind = node.kind();
    match lang {
        "rust" => {
            // impl Trait for Type → implements edge
            if kind == "impl_item" {
                // Check for "for" keyword indicating trait impl
                let full_text = node_text(node, code);
                if full_text.contains(" for ") {
                    // The trait is before "for", the type is after
                    // Tree-sitter structure: impl <trait> for <type> { ... }
                    let mut cursor = node.walk();
                    let children: Vec<_> = node.children(&mut cursor).collect();
                    // Find trait name — it's a type_identifier before the "for" keyword
                    let mut found_trait = None;
                    for child in &children {
                        if (child.kind() == "type_identifier"
                            || child.kind() == "generic_type"
                            || child.kind() == "scoped_type_identifier")
                            && found_trait.is_none()
                        {
                            found_trait = Some(node_text(child, code).to_string());
                        }
                    }
                    if let Some(trait_name) = found_trait {
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{trait_name}"),
                            kind: "implements".to_string(),
                        });
                    }
                }
            }
        }
        "python" => {
            // class Foo(Bar, Baz): → inherits edges
            if kind == "class_definition"
                && let Some(args) = find_child_by_kind(node, "argument_list")
            {
                let mut cursor = args.walk();
                for child in args.children(&mut cursor) {
                    if child.kind() == "identifier" {
                        let parent_name = node_text(&child, code).to_string();
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{parent_name}"),
                            kind: "inherits".to_string(),
                        });
                    }
                }
            }
        }
        "javascript" | "typescript" | "tsx" => {
            // class Foo extends Bar → inherits
            // class Foo implements Bar → implements (TS only)
            if kind == "class_declaration" {
                if let Some(heritage) = find_child_by_kind(node, "class_heritage") {
                    let text = node_text(&heritage, code);
                    if text.contains("extends") {
                        // Extract the parent class name
                        if let Some(id) = find_child_by_kind(&heritage, "identifier") {
                            let parent_name = node_text(&id, code).to_string();
                            edges.push(Edge {
                                from_id: sym_id.to_string(),
                                to_id: format!("__unresolved::{parent_name}"),
                                kind: "inherits".to_string(),
                            });
                        }
                    }
                }
                // TypeScript implements clause
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    let child_text = node_text(&child, code);
                    if child_text.starts_with("implements") {
                        // Extract interface names
                        let mut inner_cursor = child.walk();
                        for inner in child.children(&mut inner_cursor) {
                            if inner.kind() == "type_identifier" || inner.kind() == "identifier" {
                                let iface_name = node_text(&inner, code).to_string();
                                edges.push(Edge {
                                    from_id: sym_id.to_string(),
                                    to_id: format!("__unresolved::{iface_name}"),
                                    kind: "implements".to_string(),
                                });
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    }

    // Extract type_ref edges from signatures (all languages)
    extract_type_refs(node, code, lang, sym_id, edges);
}

/// Extract type references from function parameters, return types, and field types.
fn extract_type_refs(node: &TsNode, code: &[u8], lang: &str, sym_id: &str, edges: &mut Vec<Edge>) {
    // Collect type identifiers from the node's immediate signature area
    let type_node_kinds: &[&str] = match lang {
        "rust" => &["type_identifier", "scoped_type_identifier"],
        "python" => &["type", "identifier"], // type annotations
        "javascript" | "typescript" | "tsx" => &["type_identifier", "predefined_type"],
        "go" => &["type_identifier", "qualified_type"],
        _ => return,
    };

    // Only look at parameter lists and return types, not the full body
    let search_nodes: Vec<TsNode> = {
        let mut targets = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "parameters"
                | "parameter_list"
                | "formal_parameters"
                | "type_parameters"
                | "return_type"
                | "type_annotation"
                | "field_declaration_list"
                | "generic_type" => {
                    targets.push(child);
                }
                _ => {}
            }
        }
        targets
    };

    let mut seen = std::collections::HashSet::new();

    for search_node in &search_nodes {
        collect_type_refs_from(search_node, code, type_node_kinds, sym_id, edges, &mut seen);
    }
}

fn collect_type_refs_from(
    node: &TsNode,
    code: &[u8],
    type_kinds: &[&str],
    sym_id: &str,
    edges: &mut Vec<Edge>,
    seen: &mut std::collections::HashSet<String>,
) {
    if type_kinds.contains(&node.kind()) {
        let type_name = node_text(node, code).to_string();
        // Skip built-in/primitive types
        if !type_name.is_empty()
            && !is_primitive_type(&type_name)
            && type_name.len() < 200
            && seen.insert(type_name.clone())
        {
            edges.push(Edge {
                from_id: sym_id.to_string(),
                to_id: format!("__unresolved::{type_name}"),
                kind: "type_ref".to_string(),
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_type_refs_from(&child, code, type_kinds, sym_id, edges, seen);
    }
}

fn is_primitive_type(name: &str) -> bool {
    matches!(
        name,
        "str"
            | "String"
            | "string"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "boolean"
            | "char"
            | "int"
            | "float"
            | "complex"
            | "None"
            | "void"
            | "undefined"
            | "null"
            | "never"
            | "any"
            | "Any"
            | "object"
            | "number"
            | "Self"
            | "self"
            | "error"
            | "byte"
            | "rune"
    )
}

/// Extract decorator edges from preceding decorator nodes.
fn extract_decorator_edges(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    match lang {
        "python" => {
            // Look for decorator siblings before the function/class
            let mut sibling = node.prev_sibling();
            while let Some(sib) = sibling {
                if sib.kind() == "decorator" {
                    let text = node_text(&sib, code);
                    let decorator_name = text
                        .trim_start_matches('@')
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !decorator_name.is_empty() {
                        edges.push(Edge {
                            from_id: format!("__unresolved::{decorator_name}"),
                            to_id: sym_id.to_string(),
                            kind: "decorates".to_string(),
                        });
                        // Check for route decorators
                        if decorator_name.contains("route")
                            || decorator_name.contains(".get")
                            || decorator_name.contains(".post")
                            || decorator_name.contains(".put")
                            || decorator_name.contains(".delete")
                        {
                            let route_path = text.split(['\'', '"']).nth(1).unwrap_or("");
                            if !route_path.is_empty() {
                                edges.push(Edge {
                                    from_id: sym_id.to_string(),
                                    to_id: format!("__route::{route_path}"),
                                    kind: "routes".to_string(),
                                });
                            }
                        }
                    }
                } else if sib.kind() != "comment" {
                    break;
                }
                sibling = sib.prev_sibling();
            }
        }
        "javascript" | "typescript" | "tsx" => {
            // TS/JS decorators: @Decorator before class/method
            let mut sibling = node.prev_sibling();
            while let Some(sib) = sibling {
                if sib.kind() == "decorator" {
                    let text = node_text(&sib, code);
                    let name = text
                        .trim_start_matches('@')
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() {
                        edges.push(Edge {
                            from_id: format!("__unresolved::{name}"),
                            to_id: sym_id.to_string(),
                            kind: "decorates".to_string(),
                        });
                    }
                } else {
                    break;
                }
                sibling = sib.prev_sibling();
            }
        }
        _ => {}
    }
}

/// Extract raise/throw/panic as edges from function to error type.
fn extract_raise_edges(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    let raise_kinds: &[&str] = match lang {
        "python" => &["raise_statement"],
        "javascript" | "typescript" | "tsx" => &["throw_statement"],
        "rust" => &["macro_invocation"], // bail!, panic!, anyhow!
        "go" => &["call_expression"],    // panic()
        _ => return,
    };

    let mut cursor = node.walk();
    extract_raises_recursive(node, &mut cursor, code, lang, sym_id, edges, raise_kinds);
}

fn extract_raises_recursive(
    node: &TsNode,
    _cursor: &mut tree_sitter::TreeCursor,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
    raise_kinds: &[&str],
) {
    if raise_kinds.contains(&node.kind()) {
        let text = node_text(node, code);

        let error_name = match lang {
            "python" => {
                // raise FooError(...) or raise FooError
                text.strip_prefix("raise ")
                    .and_then(|r| r.split(|c: char| c == '(' || c.is_whitespace()).next())
                    .map(|s| s.trim().to_string())
            }
            "javascript" | "typescript" | "tsx" => {
                // throw new FooError(...)
                text.strip_prefix("throw ")
                    .and_then(|r| r.strip_prefix("new "))
                    .and_then(|r| r.split('(').next())
                    .map(|s| s.trim().to_string())
            }
            "rust" => {
                // bail!(...) or panic!(...)
                let macro_name = text.split('!').next().unwrap_or("");
                if matches!(macro_name, "bail" | "panic" | "anyhow") {
                    Some(macro_name.to_string())
                } else {
                    None
                }
            }
            "go" => {
                // panic("...")
                if text.starts_with("panic(") {
                    Some("panic".to_string())
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(name) = error_name
            && !name.is_empty()
        {
            edges.push(Edge {
                from_id: sym_id.to_string(),
                to_id: format!("__unresolved::{name}"),
                kind: "raises".to_string(),
            });
        }
        return; // Don't recurse into raise/throw children
    }

    let mut child_cursor = node.walk();
    for child in node.children(&mut child_cursor) {
        extract_raises_recursive(
            &child,
            &mut node.walk(),
            code,
            lang,
            sym_id,
            edges,
            raise_kinds,
        );
    }
}

/// Extract Go/JS route registrations: router.GET("/path", handler)
fn extract_route_registrations(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    if !matches!(lang, "go" | "javascript" | "typescript" | "tsx") {
        return;
    }

    // Look for method calls like router.GET("/path", handler) or app.get("/path", fn)
    let text = node_text(node, code);
    let http_methods = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "get", "post", "put", "delete", "patch",
    ];

    for method in &http_methods {
        let pattern = format!(".{method}(");
        if text.contains(&pattern) {
            // Extract the route path from the first string argument
            let route = text
                .split(&pattern)
                .nth(1)
                .and_then(|r| r.split(['\'', '"']).nth(1));
            if let Some(path) = route {
                edges.push(Edge {
                    from_id: sym_id.to_string(),
                    to_id: format!("__route::{path}"),
                    kind: "routes".to_string(),
                });
            }
        }
    }
}

// ─── Post-processing passes ──────────────────────────────────────────

/// Infer test edges by convention: test_foo → foo, TestFoo → Foo.
fn infer_test_edges(nodes: &[Node], edges: &mut Vec<Edge>) {
    let non_test_nodes: Vec<&Node> = nodes.iter().filter(|n| !is_test_node(n)).collect();

    for node in nodes {
        if !is_test_node(node) {
            continue;
        }

        // Try to find what this test tests
        let tested_name = extract_tested_name(&node.name);
        if let Some(name) = tested_name {
            // Find matching non-test symbol
            if let Some(target) = non_test_nodes
                .iter()
                .find(|n| n.name == name || n.name.eq_ignore_ascii_case(&name))
            {
                edges.push(Edge {
                    from_id: node.id.clone(),
                    to_id: target.id.clone(),
                    kind: "tests".to_string(),
                });
            }
        }
    }
}

fn is_test_node(node: &Node) -> bool {
    node.name.starts_with("test_")
        || node.name.starts_with("Test")
        || node.name.starts_with("test")
        || node.file_path.contains("test")
        || node.file_path.contains("spec")
}

fn extract_tested_name(test_name: &str) -> Option<String> {
    // test_foo → foo
    if let Some(name) = test_name.strip_prefix("test_") {
        return Some(name.to_string());
    }
    // TestFoo → Foo
    if let Some(name) = test_name.strip_prefix("Test")
        && name.starts_with(|c: char| c.is_uppercase())
    {
        return Some(name.to_string());
    }
    // testFoo → Foo (JS convention)
    if let Some(name) = test_name.strip_prefix("test")
        && name.starts_with(|c: char| c.is_uppercase())
    {
        return Some(name.to_string());
    }
    None
}

/// Infer override edges: if a child class has a method with the same name as parent.
fn infer_override_edges(nodes: &[Node], edges: &mut Vec<Edge>) {
    // Collect inherits relationships (clone IDs to avoid borrow conflict)
    let inherits: Vec<(String, String)> = edges
        .iter()
        .filter(|e| e.kind == "inherits")
        .map(|e| (e.from_id.clone(), e.to_id.clone()))
        .collect();

    for (child_id, parent_id) in &inherits {
        let child_methods: Vec<&Node> = nodes
            .iter()
            .filter(|n| {
                n.parent_id.as_deref() == Some(child_id.as_str())
                    && matches!(n.kind.as_str(), "function" | "method")
            })
            .collect();

        let parent_methods: Vec<&Node> = nodes
            .iter()
            .filter(|n| {
                n.parent_id.as_deref() == Some(parent_id.as_str())
                    && matches!(n.kind.as_str(), "function" | "method")
            })
            .collect();

        for child_method in &child_methods {
            if parent_methods.iter().any(|pm| pm.name == child_method.name) {
                edges.push(Edge {
                    from_id: child_method.id.clone(),
                    to_id: parent_methods
                        .iter()
                        .find(|pm| pm.name == child_method.name)
                        .unwrap()
                        .id
                        .clone(),
                    kind: "overrides".to_string(),
                });
            }
        }
    }
}

/// Infer export edges from visibility and re-export patterns.
fn infer_export_edges(nodes: &[Node], edges: &mut Vec<Edge>) {
    for node in nodes {
        if node.kind == "file" {
            continue;
        }

        // Publicly visible symbols get an "exports" edge from their file
        if matches!(node.visibility.as_str(), "pub" | "export")
            && let Some(ref parent_id) = node.parent_id
        {
            // Check if parent is a file node
            if let Some(parent) = nodes.iter().find(|n| n.id == *parent_id)
                && parent.kind == "file"
            {
                edges.push(Edge {
                    from_id: parent.id.clone(),
                    to_id: node.id.clone(),
                    kind: "exports".to_string(),
                });
            }
        }
    }
}

/// Generic names that add no semantic signal as callers/callees.
const STOPLIST: &[&str] = &[
    "new", "init", "main", "run", "build", "default", "from", "into",
    "clone", "drop", "fmt", "eq", "hash", "cmp", "test", "setup",
    "__init__", "__new__", "__repr__", "__str__", "__eq__",
    "toString", "valueOf", "constructor",
];

/// Generate natural language descriptions from graph edges.
/// Each symbol gets a templated description like:
/// "function that calls validate_token and hash_password, called by login_handler,
///  located in auth module, implements Authenticator"
fn generate_descriptions(nodes: &mut [Node], edges: &[Edge]) {
    // Build lookup maps from immutable snapshot
    let name_map: std::collections::HashMap<String, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.name.clone()))
        .collect();
    let kind_map: std::collections::HashMap<String, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.kind.clone()))
        .collect();
    let parent_map: std::collections::HashMap<String, (String, String)> = nodes
        .iter()
        .filter_map(|n| {
            n.parent_id.as_ref().and_then(|pid| {
                name_map.get(pid).and_then(|pname| {
                    kind_map.get(pid).map(|pkind| {
                        (n.id.clone(), (pname.clone(), pkind.clone()))
                    })
                })
            })
        })
        .collect();

    // Pre-compute edge lookups
    let mut calls_out: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut called_by: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut implements: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut inherits_from: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut type_refs: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut tested_by: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
    let mut decorators: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();

    for edge in edges {
        match edge.kind.as_str() {
            "calls" => {
                calls_out.entry(edge.from_id.as_str()).or_default().push(&edge.to_id);
                called_by.entry(edge.to_id.as_str()).or_default().push(&edge.from_id);
            }
            "implements" => {
                implements.entry(edge.from_id.as_str()).or_default().push(&edge.to_id);
            }
            "inherits" => {
                inherits_from.entry(edge.from_id.as_str()).or_default().push(&edge.to_id);
            }
            "type_ref" => {
                type_refs.entry(edge.from_id.as_str()).or_default().push(&edge.to_id);
            }
            "tests" => {
                tested_by.entry(edge.to_id.as_str()).or_default().push(&edge.from_id);
            }
            "decorates" => {
                decorators.entry(edge.to_id.as_str()).or_default().push(&edge.from_id);
            }
            _ => {}
        }
    }

    for node in nodes.iter_mut() {
        if node.kind == "file" || node.kind == "doc_section" {
            continue;
        }

        let mut parts: Vec<String> = Vec::new();

        // Kind + name
        parts.push(format!("{} {}", node.kind, node.name));

        // Parent context
        if let Some((pname, pkind)) = parent_map.get(&node.id) {
            if pkind != "file" {
                parts.push(format!("in {pkind} {pname}"));
            } else {
                parts.push(format!("in {pname}"));
            }
        }

        // Calls (filtered by stoplist)
        if let Some(callees) = calls_out.get(node.id.as_str()) {
            let names: Vec<&str> = callees
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| !STOPLIST.contains(n) && n.len() > 1)
                .take(5)
                .collect();
            if !names.is_empty() {
                parts.push(format!("calls {}", names.join(" ")));
            }
        }

        // Called by (callers are semantic signal)
        if let Some(callers) = called_by.get(node.id.as_str()) {
            let names: Vec<&str> = callers
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| !STOPLIST.contains(n) && n.len() > 1)
                .take(5)
                .collect();
            if !names.is_empty() {
                parts.push(format!("called by {}", names.join(" ")));
            }
        }

        // Implements
        if let Some(traits) = implements.get(node.id.as_str()) {
            let names: Vec<&str> = traits
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .collect();
            if !names.is_empty() {
                parts.push(format!("implements {}", names.join(" ")));
            }
        }

        // Inherits
        if let Some(parents) = inherits_from.get(node.id.as_str()) {
            let names: Vec<&str> = parents
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .collect();
            if !names.is_empty() {
                parts.push(format!("extends {}", names.join(" ")));
            }
        }

        // Decorators
        if let Some(decs) = decorators.get(node.id.as_str()) {
            let names: Vec<&str> = decs
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| !STOPLIST.contains(n))
                .take(3)
                .collect();
            if !names.is_empty() {
                parts.push(format!("decorated with {}", names.join(" ")));
            }
        }

        // Type references
        if let Some(refs) = type_refs.get(node.id.as_str()) {
            let names: Vec<&str> = refs
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| n.len() > 2)
                .take(3)
                .collect();
            if !names.is_empty() {
                parts.push(format!("uses {}", names.join(" ")));
            }
        }

        // Tested by
        if let Some(tests) = tested_by.get(node.id.as_str()) {
            let names: Vec<&str> = tests
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .take(2)
                .collect();
            if !names.is_empty() {
                parts.push(format!("tested by {}", names.join(" ")));
            }
        }

        node.description = Some(parts.join(", "));
    }
}

/// Extract import/use statements as edges.
fn extract_imports(
    root: &TsNode,
    code: &[u8],
    lang: &str,
    source_name: &str,
    edges: &mut Vec<Edge>,
) {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let kind = child.kind();
        match lang {
            "rust" if kind == "use_declaration" => {
                // use foo::bar::Baz;
                let text = node_text(&child, code);
                let imported = text.trim_start_matches("use ").trim_end_matches(';').trim();
                if !imported.is_empty() {
                    let file_id = Node::id_for(source_name, &format!("{source_name}::{imported}"));
                    edges.push(Edge {
                        from_id: String::new(), // resolved later
                        to_id: format!("__unresolved::{imported}"),
                        kind: "imports".to_string(),
                    });
                    let _ = file_id; // suppress unused
                }
            }
            "python" if kind == "import_statement" || kind == "import_from_statement" => {
                let text = node_text(&child, code);
                let imported = text
                    .trim_start_matches("from ")
                    .trim_start_matches("import ")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if !imported.is_empty() {
                    edges.push(Edge {
                        from_id: String::new(),
                        to_id: format!("__unresolved::{imported}"),
                        kind: "imports".to_string(),
                    });
                }
            }
            "javascript" | "typescript" | "tsx" if kind == "import_statement" => {
                // import { foo } from 'bar'
                if let Some(source_node) = find_child_by_kind(&child, "string") {
                    let module = node_text(&source_node, code)
                        .trim_matches(|c| c == '\'' || c == '"')
                        .to_string();
                    if !module.is_empty() {
                        edges.push(Edge {
                            from_id: String::new(),
                            to_id: format!("__unresolved::{module}"),
                            kind: "imports".to_string(),
                        });
                    }
                }
            }
            "go" if kind == "import_declaration" => {
                let text = node_text(&child, code);
                for line in text.lines() {
                    let cleaned = line.trim().trim_matches('"');
                    if !cleaned.is_empty()
                        && cleaned != "import"
                        && cleaned != "("
                        && cleaned != ")"
                    {
                        edges.push(Edge {
                            from_id: String::new(),
                            to_id: format!("__unresolved::{cleaned}"),
                            kind: "imports".to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Iterative AST traversal — avoids stack overflow on deeply nested code.
fn extract_node(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    file_path: &str,
    source_name: &str,
    source_version: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    parent_id: Option<&str>,
    prefix: &str,
) {
    let kind = node.kind();

    // Try to extract a node from this node
    if let Some(mut sym) = match lang {
        "rust" => extract_rust_node(node, code, kind),
        "python" => extract_python_node(node, code, kind),
        "javascript" | "typescript" | "tsx" => extract_js_node(node, code, kind),
        "go" => extract_go_node(node, code, kind),
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
        sym.language = lang.to_string();
        sym.file_path = file_path.to_string();
        sym.start_line = node.start_position().row + 1;
        sym.start_col = node.start_position().column;
        sym.end_line = node.end_position().row + 1;
        sym.line_count = sym.end_line.saturating_sub(sym.start_line) + 1;
        sym.visibility = detect_visibility(node, code, lang);
        sym.id = Node::id_for(source_name, &sym.qualified_name);
        sym.parent_id = parent_id.map(|s| s.to_string());

        // Content hash for staleness detection
        let source_text = &code[node.start_byte()..node.end_byte()];
        sym.content_hash = Some(blake3::hash(source_text).to_hex().to_string());

        sym.body = sym.build_body();

        let sym_id = sym.id.clone();
        let sym_kind = sym.kind.clone();

        nodes.push(sym);

        // Extract relationship edges
        extract_relationship_edges(node, code, lang, &sym_id, edges);
        extract_decorator_edges(node, code, lang, &sym_id, edges);

        // For functions/methods: extract raises and route registrations
        if matches!(sym_kind.as_str(), "function" | "method") {
            extract_raise_edges(node, code, lang, &sym_id, edges);
            extract_route_registrations(node, code, lang, &sym_id, edges);
        }

        // Recurse into children with this node as parent
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
                nodes,
                edges,
                child_parent,
                &new_prefix,
            );
        }
    } else {
        // Not a node node — recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            extract_node(
                &child,
                code,
                lang,
                file_path,
                source_name,
                source_version,
                nodes,
                edges,
                parent_id,
                prefix,
            );
        }
    }
}

/// Extract identifier references from a function body as potential "calls" edges.
fn extract_call_references(node: &TsNode, code: &[u8], caller_id: &str, edges: &mut Vec<Edge>) {
    let mut cursor = node.walk();
    extract_calls_recursive(node, &mut cursor, code, caller_id, edges);
}

fn extract_calls_recursive(
    node: &TsNode,
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
                // Store as an unresolved reference — we'll resolve to actual node IDs later
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

/// Resolve unresolved reference edges to actual node IDs.
fn resolve_references(edges: &mut Vec<Edge>, nodes: &[Node]) {
    for edge in edges.iter_mut() {
        if let Some(ref_name) = edge.to_id.strip_prefix("__unresolved::") {
            // Try to find a matching node by name
            if let Some(target) = nodes.iter().find(|s| {
                s.name == ref_name || s.qualified_name.ends_with(&format!("::{ref_name}"))
            }) {
                edge.to_id = target.id.clone();
            }
        }
    }

    // Remove edges that couldn't be resolved (external calls, stdlib, etc.)
    edges.retain(|e| !e.to_id.starts_with("__unresolved::"));
}

// ─── Markdown documentation extraction ───────────────────────────────

/// Extract documentation nodes from a Markdown file.
/// Creates a file node, section nodes (split on headings), and references edges
/// for backtick-quoted identifiers.
fn extract_markdown_doc(
    content: &str,
    rel_path: &str,
    source_name: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    // Create file node
    let file_qualified = format!("{source_name}::{rel_path}");
    let file_id = Node::id_for(source_name, &file_qualified);
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let file_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let file_lines = content.lines().count();

    nodes.push(make_file_node(
        &file_id,
        file_name,
        &file_qualified,
        source_name,
        "markdown",
        rel_path,
        Some(&file_hash),
        file_lines,
    ));

    // Parse into sections by headings
    let mut current_heading: Option<String> = None;
    let mut current_body = String::new();
    let mut section_start_line = 1usize;

    for (i, line) in content.lines().enumerate() {
        if let Some(heading) = parse_md_heading(line) {
            // Flush previous section
            flush_doc_section(
                &current_heading,
                &current_body,
                section_start_line,
                rel_path,
                source_name,
                &file_id,
                nodes,
                edges,
            );
            current_heading = Some(heading);
            current_body.clear();
            section_start_line = i + 1;
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // Flush final section
    flush_doc_section(
        &current_heading,
        &current_body,
        section_start_line,
        rel_path,
        source_name,
        &file_id,
        nodes,
        edges,
    );
}

fn parse_md_heading(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if level > 6 {
        return None;
    }
    let text = trimmed[level..].trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn flush_doc_section(
    heading: &Option<String>,
    body: &str,
    start_line: usize,
    file_path: &str,
    source_name: &str,
    file_id: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }

    let section_name = heading.as_deref().unwrap_or("(preamble)");
    let qualified = format!("{source_name}::{file_path}::{section_name}");
    let id = Node::id_for(source_name, &qualified);

    let body_text = format!("doc_section: {qualified}\n{section_name}\n{trimmed}");

    nodes.push(Node {
        id: id.clone(),
        kind: "doc_section".to_string(),
        name: section_name.to_string(),
        qualified_name: qualified,
        source_name: source_name.to_string(),
        language: "markdown".to_string(),
        file_path: file_path.to_string(),
        start_line,
        start_col: 0,
        end_line: 0,
        visibility: String::new(),
        signature: None,
        doc: Some(trimmed.to_string()),
        body: body_text,
        parent_id: Some(file_id.to_string()),
        content_hash: Some(blake3::hash(trimmed.as_bytes()).to_hex().to_string()),
        line_count: trimmed.lines().count(),
        source_url: None,
        description: None,
    });

    // Extract backtick references as edges to code symbols
    for cap in extract_backtick_refs(trimmed) {
        edges.push(Edge {
            from_id: id.clone(),
            to_id: format!("__unresolved::{cap}"),
            kind: "references".to_string(),
        });
    }
}

/// Extract identifiers from backtick-quoted text in markdown.
fn extract_backtick_refs(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut in_backtick = false;
    let mut current = String::new();

    for ch in text.chars() {
        if ch == '`' {
            if in_backtick {
                // End of backtick — check if it looks like an identifier
                let trimmed = current.trim();
                if !trimmed.is_empty()
                    && trimmed.len() < 100
                    && !trimmed.contains(' ')
                    // Skip things that look like code snippets, not identifiers
                    && !trimmed.contains('=')
                    && !trimmed.starts_with('-')
                    && !trimmed.starts_with('$')
                {
                    // Take the last component of a qualified name
                    let name = trimmed.rsplit([':', '.', '/']).next().unwrap_or(trimmed);
                    if !name.is_empty()
                        && name
                            .chars()
                            .next()
                            .map(|c| c.is_alphabetic())
                            .unwrap_or(false)
                    {
                        refs.push(name.to_string());
                    }
                }
                current.clear();
                in_backtick = false;
            } else {
                in_backtick = true;
            }
        } else if in_backtick {
            current.push(ch);
        }
    }

    refs.sort();
    refs.dedup();
    refs
}

// ─── Language-specific node extraction ───────────────────────────────

fn node_text<'a>(node: &TsNode, code: &'a [u8]) -> &'a str {
    node.utf8_text(code).unwrap_or("")
}

fn find_child_by_kind<'a>(node: &TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

fn extract_doc_comment(node: &TsNode, code: &[u8]) -> Option<String> {
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

fn extract_python_docstring(node: &TsNode, code: &[u8]) -> Option<String> {
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

fn extract_rust_node(node: &TsNode, code: &[u8], kind: &str) -> Option<Node> {
    match kind {
        "function_item" | "function_signature_item" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "function", sig, doc))
        }
        "struct_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(
                &name,
                "struct",
                Some(format!("struct {}", &name)),
                doc,
            ))
        }
        "enum_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(
                &name,
                "enum",
                Some(format!("enum {}", &name)),
                doc,
            ))
        }
        "trait_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(
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
            let sig = extract_signature_text(node, code);
            Some(make_node(&name, "impl", sig, None))
        }
        "mod_item" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "module", None, doc))
        }
        "const_item" | "static_item" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(
                &name,
                "const",
                Some(format!("const {name}")),
                doc,
            ))
        }
        "type_item" => {
            let name = node_text(&find_child_by_kind(node, "type_identifier")?, code).to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "type", Some(format!("type {name}")), doc))
        }
        _ => None,
    }
}

fn extract_python_node(node: &TsNode, code: &[u8], kind: &str) -> Option<Node> {
    match kind {
        "function_definition" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            if name.starts_with('_') && !name.starts_with("__") {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc =
                extract_python_docstring(node, code).or_else(|| extract_doc_comment(node, code));
            Some(make_node(&name, "function", sig, doc))
        }
        "class_definition" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let doc =
                extract_python_docstring(node, code).or_else(|| extract_doc_comment(node, code));
            Some(make_node(
                &name,
                "class",
                Some(format!("class {name}")),
                doc,
            ))
        }
        _ => None,
    }
}

fn extract_js_node(node: &TsNode, code: &[u8], kind: &str) -> Option<Node> {
    match kind {
        "function_declaration" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "function", sig, doc))
        }
        "class_declaration" => {
            let name = node_text(
                &find_child_by_kind(node, "identifier")
                    .or_else(|| find_child_by_kind(node, "type_identifier"))?,
                code,
            )
            .to_string();
            let doc = extract_doc_comment(node, code);
            Some(make_node(
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
                if let Some(sym) = extract_js_node(&child, code, child.kind()) {
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
            Some(make_node(&name, "method", sig, doc))
        }
        "lexical_declaration" => {
            // export const foo = (...) => ...
            let decl = find_child_by_kind(node, "variable_declarator")?;
            let name = node_text(&find_child_by_kind(&decl, "identifier")?, code).to_string();
            let value = decl.child_by_field_name("value")?;
            if value.kind() == "arrow_function" || value.kind() == "function" {
                let doc = extract_doc_comment(node, code);
                Some(make_node(&name, "function", None, doc))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn extract_go_node(node: &TsNode, code: &[u8], kind: &str) -> Option<Node> {
    match kind {
        "function_declaration" => {
            let name = node_text(&find_child_by_kind(node, "identifier")?, code).to_string();
            // Only export uppercase functions in Go
            if !name.starts_with(|c: char| c.is_uppercase()) {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "function", sig, doc))
        }
        "method_declaration" => {
            let name = node_text(&find_child_by_kind(node, "field_identifier")?, code).to_string();
            if !name.starts_with(|c: char| c.is_uppercase()) {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "method", sig, doc))
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
            Some(make_node(&name, type_kind, None, doc))
        }
        _ => None,
    }
}

fn detect_visibility(node: &TsNode, code: &[u8], lang: &str) -> String {
    match lang {
        "rust" => {
            if find_child_by_kind(node, "visibility_modifier").is_some() {
                "pub".to_string()
            } else {
                "private".to_string()
            }
        }
        "go" => {
            // Go exports uppercase names
            let name = find_child_by_kind(node, "identifier")
                .or_else(|| find_child_by_kind(node, "field_identifier"))
                .or_else(|| find_child_by_kind(node, "type_identifier"));
            if let Some(n) = name {
                let text = node_text(&n, code);
                if text.starts_with(|c: char| c.is_uppercase()) {
                    "pub".to_string()
                } else {
                    "private".to_string()
                }
            } else {
                String::new()
            }
        }
        "javascript" | "typescript" | "tsx" => {
            // Check if parent is an export_statement
            if let Some(parent) = node.parent()
                && parent.kind() == "export_statement"
            {
                return "export".to_string();
            }
            "private".to_string()
        }
        "python" => {
            if let Some(n) = find_child_by_kind(node, "identifier") {
                let text = node_text(&n, code);
                if text.starts_with('_') && !text.starts_with("__") {
                    "private".to_string()
                } else {
                    "pub".to_string()
                }
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn make_file_node(
    id: &str,
    name: &str,
    qualified_name: &str,
    source_name: &str,
    language: &str,
    file_path: &str,
    content_hash: Option<&str>,
    line_count: usize,
) -> Node {
    Node {
        id: id.to_string(),
        kind: "file".to_string(),
        name: name.to_string(),
        qualified_name: qualified_name.to_string(),
        source_name: source_name.to_string(),
        language: language.to_string(),
        file_path: file_path.to_string(),
        start_line: 0,
        start_col: 0,
        end_line: 0,
        visibility: String::new(),
        signature: None,
        doc: None,
        body: format!("file: {file_path}"),
        parent_id: None,
        content_hash: content_hash.map(|s| s.to_string()),
        line_count,
        source_url: None,
        description: None,
    }
}

fn make_node(name: &str, kind: &str, signature: Option<String>, doc: Option<String>) -> Node {
    let name = name.to_string();
    Node {
        id: String::new(),
        kind: kind.to_string(),
        name,
        qualified_name: String::new(),
        source_name: String::new(),
        language: String::new(),
        file_path: String::new(),
        start_line: 0,
        start_col: 0,
        end_line: 0,
        visibility: String::new(),
        signature,
        doc,
        body: String::new(),
        parent_id: None,
        content_hash: None,
        line_count: 0,
        source_url: None,
        description: None,
    }
}

/// Extract the signature line(s) from a node — everything up to the body.
fn extract_signature_text(node: &TsNode, code: &[u8]) -> Option<String> {
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
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            "test.rs",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph { nodes, edges }
    }

    fn extract_python(code: &str) -> FileGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_python::LANGUAGE.into(),
            "python",
            "test.py",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph { nodes, edges }
    }

    fn extract_js(code: &str) -> FileGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            "test.js",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph { nodes, edges }
    }

    #[test]
    fn test_rust_function() {
        let g = extract_rust("pub fn spawn() {}");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].name, "spawn");
        assert_eq!(g.nodes[0].kind, "function");
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
        let names: Vec<&str> = g.nodes.iter().map(|s| s.name.as_str()).collect();
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
        let kinds: Vec<&str> = g.nodes.iter().map(|s| s.kind.as_str()).collect();
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
        assert_eq!(g.nodes.len(), 1);
        assert!(
            g.nodes[0]
                .doc
                .as_deref()
                .unwrap()
                .contains("Spawns a new task"),
            "doc: {:?}",
            g.nodes[0].doc
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
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].name, "greet");
        assert!(g.nodes[0].doc.as_deref().unwrap().contains("Say hello"));
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
        let names: Vec<&str> = g.nodes.iter().map(|s| s.name.as_str()).collect();
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
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].name, "greet");
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
        let names: Vec<&str> = g.nodes.iter().map(|s| s.name.as_str()).collect();
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
        let login = g.nodes.iter().find(|s| s.name == "login").unwrap();
        assert!(
            login.parent_id.is_some(),
            "login should have a parent (the auth module)"
        );
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
        let login = g.nodes.iter().find(|s| s.name == "login").unwrap();
        assert!(
            login.qualified_name.contains("auth::login"),
            "got: {}",
            login.qualified_name
        );
    }

    #[test]
    fn test_extract_dir_with_file_nodes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn hello() {}\npub fn world() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        let file_nodes: Vec<_> = g.nodes.iter().filter(|n| n.kind == "file").collect();
        assert_eq!(file_nodes.len(), 1, "should have one file node");
        assert_eq!(file_nodes[0].name, "lib.rs");

        // Functions should have the file as parent
        let hello = g.nodes.iter().find(|n| n.name == "hello").unwrap();
        assert_eq!(hello.parent_id, Some(file_nodes[0].id.clone()));
    }

    #[test]
    fn test_visibility_detection() {
        let g = extract_rust(
            r#"
            pub fn public_fn() {}
            fn private_fn() {}
            "#,
        );
        let public = g.nodes.iter().find(|n| n.name == "public_fn").unwrap();
        assert_eq!(public.visibility, "pub");

        let private = g.nodes.iter().find(|n| n.name == "private_fn").unwrap();
        assert_eq!(private.visibility, "private");
    }

    #[test]
    fn test_rust_implements_edge() {
        let g = extract_rust(
            r#"
            pub trait Serialize {}
            pub struct Foo {}
            impl Serialize for Foo {}
            "#,
        );
        let impl_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "implements").collect();
        assert!(
            !impl_edges.is_empty(),
            "should have implements edge from impl Serialize for Foo"
        );
    }

    #[test]
    fn test_python_inherits_edge() {
        let g = extract_python(
            r#"
class Base:
    """Base class."""
    pass

class Child(Base):
    """Child class."""
    pass
            "#,
        );
        let inherits: Vec<_> = g.edges.iter().filter(|e| e.kind == "inherits").collect();
        assert!(
            !inherits.is_empty(),
            "should have inherits edge from Child to Base"
        );
    }

    #[test]
    fn test_js_extends_edge() {
        let g = extract_js(
            r#"
            class Animal {}
            class Dog extends Animal {
                bark() {}
            }
            "#,
        );
        let inherits: Vec<_> = g.edges.iter().filter(|e| e.kind == "inherits").collect();
        assert!(
            !inherits.is_empty(),
            "should have inherits edge from Dog to Animal"
        );
    }

    #[test]
    fn test_python_visibility() {
        let g = extract_python(
            r#"
def public():
    """Public."""
    pass

def _private():
    """Private."""
    pass
            "#,
        );
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        // _private should be skipped by extractor (starts with _)
        assert!(names.contains(&"public"));
    }

    #[test]
    fn test_rust_type_ref_edges() {
        let g = extract_rust(
            r#"
            pub struct Config {}
            pub fn load(path: Config) -> Result {}
            "#,
        );
        let type_refs: Vec<_> = g.edges.iter().filter(|e| e.kind == "type_ref").collect();
        assert!(
            !type_refs.is_empty(),
            "should have type_ref edge for Config param: {:?}",
            g.edges
        );
    }

    #[test]
    fn test_markdown_doc_extraction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "# Getting Started\nInstall with `cargo`.\n\n## Usage\nCall `add` to ingest sources.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn add() {}\n").unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();

        // Should have doc_section nodes
        let doc_sections: Vec<_> = g.nodes.iter().filter(|n| n.kind == "doc_section").collect();
        assert!(
            doc_sections.len() >= 2,
            "should have at least 2 doc sections (Getting Started, Usage), got {}",
            doc_sections.len()
        );

        // Should have references edges from backtick mentions
        let refs: Vec<_> = g.edges.iter().filter(|e| e.kind == "references").collect();
        assert!(
            !refs.is_empty(),
            "should have references edges from backtick mentions"
        );
    }

    #[test]
    fn test_backtick_ref_extraction() {
        let refs = extract_backtick_refs("Use `spawn` to create tasks. See `tokio::Runtime`.");
        assert!(refs.contains(&"spawn".to_string()));
        assert!(refs.contains(&"Runtime".to_string()));
    }

    #[test]
    fn test_python_decorator_edge() {
        let g = extract_python(
            r#"
def cache(fn):
    """Cache decorator."""
    pass

@cache
def expensive():
    """Expensive computation."""
    pass
            "#,
        );
        let decorates: Vec<_> = g.edges.iter().filter(|e| e.kind == "decorates").collect();
        assert!(
            !decorates.is_empty(),
            "should have decorates edge: {:?}",
            g.edges
        );
    }

    #[test]
    fn test_test_edges_inferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn spawn() {}\npub fn test_spawn() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        let test_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "tests").collect();
        assert!(
            !test_edges.is_empty(),
            "should infer test_spawn tests spawn"
        );
    }

    #[test]
    fn test_export_edges_inferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn public_api() {}\nfn private_impl() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        let exports: Vec<_> = g.edges.iter().filter(|e| e.kind == "exports").collect();
        assert!(
            !exports.is_empty(),
            "pub functions should get exports edges from file"
        );
    }

    #[test]
    fn test_backtick_skips_non_identifiers() {
        let refs = extract_backtick_refs("Run `cargo build --release` and `export PATH=$HOME`.");
        // These contain spaces, =, or $ — should be skipped
        assert!(refs.is_empty(), "should skip non-identifiers: {:?}", refs);
    }
}
