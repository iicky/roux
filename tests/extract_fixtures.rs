//! Fixture-driven extractor tests.
//!
//! Inline string literals in `src/graph/extract.rs` cover happy-path
//! parsing for tiny snippets. These tests run the extractor against
//! realistic mini-projects and adversarial inputs checked into
//! `tests/fixtures/`. Assertions are presence-based ("contains symbol X")
//! rather than count-based, so adding fields or improving extraction
//! does not break them.

use std::path::Path;

use roux_cli::graph::extract::extract_dir;
use roux_cli::graph::{Edge, Node};

fn fixture(rel: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

fn names(nodes: &[Node]) -> Vec<&str> {
    nodes.iter().map(|n| n.name.as_str()).collect()
}

fn names_of_kind<'a>(nodes: &'a [Node], kind: &str) -> Vec<&'a str> {
    nodes
        .iter()
        .filter(|n| n.kind == kind)
        .map(|n| n.name.as_str())
        .collect()
}

fn edges_of_kind<'a>(edges: &'a [Edge], kind: &str) -> Vec<&'a Edge> {
    edges.iter().filter(|e| e.kind == kind).collect()
}

fn id_to_name(nodes: &[Node]) -> std::collections::HashMap<&str, &str> {
    nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect()
}

// ─── Rust: realistic mini-crate ────────────────────────────────────

#[test]
fn rust_basic_extracts_module_structure() {
    let g = extract_dir(&fixture("rust/basic"), "tiny-graph", "0.1.0", Some("rust"))
        .expect("extract should succeed on valid fixture");

    let all = names(&g.nodes);

    // Cross-file structs and enums
    assert!(all.contains(&"Node"), "missing struct Node — got {all:?}");
    assert!(all.contains(&"Graph"), "missing struct Graph");
    assert!(all.contains(&"GraphError"), "missing enum GraphError");

    // Builder + trait
    assert!(
        all.contains(&"ParserBuilder"),
        "missing struct ParserBuilder"
    );
    assert!(all.contains(&"Parse"), "missing trait Parse");

    // Methods on impls
    let methods = names_of_kind(&g.nodes, "method");
    assert!(
        methods.contains(&"new"),
        "missing method new — got {methods:?}"
    );
    assert!(methods.contains(&"add_node"));
    assert!(methods.contains(&"add_edge"));
    assert!(methods.contains(&"strict"));

    // File nodes — one per source file
    let files = names_of_kind(&g.nodes, "file");
    assert!(files.contains(&"lib.rs"));
    assert!(files.contains(&"parser.rs"));
    assert!(files.contains(&"types.rs"));
}

#[test]
fn rust_basic_extracts_trait_and_impl_edges() {
    let g = extract_dir(&fixture("rust/basic"), "tiny-graph", "0.1.0", Some("rust"))
        .expect("extract should succeed");

    let implements = edges_of_kind(&g.edges, "implements");
    let id_name = id_to_name(&g.nodes);

    // `impl Parse for ParserBuilder` should produce a ParserBuilder→Parse edge.
    let has_parse_impl = implements.iter().any(|e| {
        let from = id_name.get(e.from_id.as_str()).copied().unwrap_or("");
        let to = id_name.get(e.to_id.as_str()).copied().unwrap_or("");
        from == "ParserBuilder" && to == "Parse"
    });
    assert!(
        has_parse_impl,
        "expected ParserBuilder implements Parse edge — got {} implements edges",
        implements.len()
    );
}

#[test]
fn rust_basic_extracts_call_edges() {
    let g = extract_dir(&fixture("rust/basic"), "tiny-graph", "0.1.0", Some("rust"))
        .expect("extract should succeed");

    let calls = edges_of_kind(&g.edges, "calls");
    assert!(
        !calls.is_empty(),
        "expected call edges (e.g. add_node, contains_key) — got 0"
    );
}

// ─── Rust: adversarial inputs ──────────────────────────────────────

#[test]
fn rust_adversarial_does_not_panic() {
    // Just running this without panicking is half the test. The other half
    // is that file-level metadata is still produced for every file we read.
    let g = extract_dir(
        &fixture("rust/adversarial"),
        "rust-adversarial",
        "0.1.0",
        Some("rust"),
    )
    .expect("extract should not error on a directory of malformed files");

    let files = names_of_kind(&g.nodes, "file");
    // We have 6 .rs files; tree-sitter recovery still gives us file nodes.
    for expected in [
        "malformed.rs",
        "empty.rs",
        "unicode_idents.rs",
        "huge_line.rs",
        "bom.rs",
        "crlf.rs",
    ] {
        assert!(
            files.contains(&expected),
            "expected file node for {expected} — got {files:?}"
        );
    }
}

#[test]
fn rust_adversarial_unicode_identifiers_extracted() {
    let g = extract_dir(
        &fixture("rust/adversarial"),
        "rust-adversarial",
        "0.1.0",
        Some("rust"),
    )
    .expect("extract should succeed");

    let all = names(&g.nodes);
    // Whatever tree-sitter-rust does with non-ASCII, it should not panic
    // and should ideally surface at least one of these as a symbol.
    let saw_unicode = all
        .iter()
        .any(|n| n.contains("αβγ") || n.contains("Δelta") || n.contains("日本語_function"));
    assert!(
        saw_unicode,
        "expected at least one unicode identifier extracted from unicode_idents.rs — got {all:?}"
    );
}

#[test]
fn rust_adversarial_crlf_and_bom_handled() {
    let g = extract_dir(
        &fixture("rust/adversarial"),
        "rust-adversarial",
        "0.1.0",
        Some("rust"),
    )
    .expect("extract should succeed");

    let all = names(&g.nodes);
    assert!(
        all.contains(&"with_crlf"),
        "CRLF-line-ending file should still extract its function — got {all:?}"
    );
    assert!(
        all.contains(&"with_bom"),
        "UTF-8 BOM file should still extract its function — got {all:?}"
    );
}

#[test]
fn rust_adversarial_huge_line_does_not_explode() {
    // 200-element vec literal on one line — common in real generated code.
    let g = extract_dir(
        &fixture("rust/adversarial"),
        "rust-adversarial",
        "0.1.0",
        Some("rust"),
    )
    .expect("extract should succeed");

    let all = names(&g.nodes);
    assert!(
        all.contains(&"huge"),
        "expected `huge` function from huge_line.rs"
    );
}

// ─── Python: realistic mini-package ────────────────────────────────

#[test]
fn python_basic_extracts_classes_and_inheritance() {
    let g = extract_dir(
        &fixture("python/basic"),
        "tiny-py-pkg",
        "0.1.0",
        Some("python"),
    )
    .expect("extract should succeed");

    let all = names(&g.nodes);

    assert!(all.contains(&"User"), "missing class User — got {all:?}");
    assert!(all.contains(&"AdminUser"), "missing class AdminUser");
    assert!(all.contains(&"display_name"), "missing method display_name");
    assert!(all.contains(&"can_edit"), "missing method can_edit");
    assert!(
        all.contains(&"handle_request"),
        "missing function handle_request"
    );

    // File nodes for all three files — package layout.
    let files = names_of_kind(&g.nodes, "file");
    assert!(files.contains(&"__init__.py"));
    assert!(files.contains(&"models.py"));
    assert!(files.contains(&"handlers.py"));
}

#[test]
fn python_basic_extracts_inherits_edge() {
    let g = extract_dir(
        &fixture("python/basic"),
        "tiny-py-pkg",
        "0.1.0",
        Some("python"),
    )
    .expect("extract should succeed");

    let inherits = edges_of_kind(&g.edges, "inherits");
    let id_name = id_to_name(&g.nodes);

    // class AdminUser(User): ... should produce AdminUser→User
    let has_admin_inherits = inherits.iter().any(|e| {
        let from = id_name.get(e.from_id.as_str()).copied().unwrap_or("");
        let to = id_name.get(e.to_id.as_str()).copied().unwrap_or("");
        from == "AdminUser" && to == "User"
    });
    assert!(
        has_admin_inherits,
        "expected AdminUser inherits User edge — got {} inherits edges",
        inherits.len()
    );
}

// ─── Python: adversarial ───────────────────────────────────────────

#[test]
fn python_adversarial_does_not_panic() {
    let g = extract_dir(
        &fixture("python/adversarial"),
        "py-adversarial",
        "0.1.0",
        Some("python"),
    )
    .expect("extract should not error on adversarial dir");

    let files = names_of_kind(&g.nodes, "file");
    for expected in [
        "malformed.py",
        "empty.py",
        "unicode_idents.py",
        "bom.py",
        "crlf.py",
    ] {
        assert!(
            files.contains(&expected),
            "expected file node for {expected} — got {files:?}"
        );
    }
}

#[test]
fn python_adversarial_unicode_identifiers_extracted() {
    let g = extract_dir(
        &fixture("python/adversarial"),
        "py-adversarial",
        "0.1.0",
        Some("python"),
    )
    .expect("extract should succeed");

    let all = names(&g.nodes);
    let saw_unicode = all
        .iter()
        .any(|n| n.contains("αβγ") || n.contains("Δelta") || n.contains("日本語_function"));
    assert!(
        saw_unicode,
        "expected at least one unicode identifier extracted — got {all:?}"
    );
}

#[test]
fn python_adversarial_crlf_and_bom_handled() {
    let g = extract_dir(
        &fixture("python/adversarial"),
        "py-adversarial",
        "0.1.0",
        Some("python"),
    )
    .expect("extract should succeed");

    let all = names(&g.nodes);
    assert!(
        all.contains(&"with_crlf"),
        "CRLF Python file should still extract its function"
    );
    assert!(
        all.contains(&"with_bom"),
        "BOM Python file should still extract its function"
    );
}
