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
    let g = extract_dir(&fixture("rust/basic"), "tiny-graph", Some("rust"))
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
    let g = extract_dir(&fixture("rust/basic"), "tiny-graph", Some("rust"))
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
    let g = extract_dir(&fixture("rust/basic"), "tiny-graph", Some("rust"))
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
    let g = extract_dir(&fixture("python/basic"), "tiny-py-pkg", Some("python"))
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
    let g = extract_dir(&fixture("python/basic"), "tiny-py-pkg", Some("python"))
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

// ─── TypeScript: realistic mini-project ────────────────────────────

#[test]
fn typescript_basic_extracts_classes_interfaces_types() {
    let g = extract_dir(&fixture("typescript/basic"), "tiny-ts", Some("typescript"))
        .expect("extract should succeed");

    let all = names(&g.nodes);

    // Interfaces (added in roux-hx9)
    assert!(
        all.contains(&"User"),
        "missing interface User — got {all:?}"
    );
    assert!(all.contains(&"AdminUser"), "missing interface AdminUser");

    // Type aliases (added in roux-hx9)
    assert!(all.contains(&"UserRole"), "missing type alias UserRole");

    // Classes and functions (already worked)
    assert!(all.contains(&"UserStore"), "missing class UserStore");
    assert!(
        all.contains(&"RequestHandler"),
        "missing class RequestHandler"
    );
    assert!(
        all.contains(&"fetchUser"),
        "missing async function fetchUser"
    );
    assert!(all.contains(&"isAdmin"), "missing user-defined type guard");

    let files = names_of_kind(&g.nodes, "file");
    assert!(files.contains(&"types.ts"));
    assert!(files.contains(&"handlers.ts"));
    assert!(files.contains(&"index.ts"));
}

#[test]
fn typescript_adversarial_does_not_panic() {
    let g = extract_dir(
        &fixture("typescript/adversarial"),
        "ts-adversarial",
        Some("typescript"),
    )
    .expect("extract should not error");

    let files = names_of_kind(&g.nodes, "file");
    for expected in ["malformed.ts", "empty.ts", "bom.ts", "crlf.ts"] {
        assert!(
            files.contains(&expected),
            "expected file node for {expected} — got {files:?}"
        );
    }

    let all = names(&g.nodes);
    assert!(
        all.contains(&"withCrlf"),
        "CRLF .ts should still extract — got {all:?}"
    );
    assert!(
        all.contains(&"withBom"),
        "BOM .ts should still extract — got {all:?}"
    );
}

// ─── Go: realistic package ─────────────────────────────────────────

#[test]
fn go_basic_extracts_structs_interfaces_methods() {
    let g =
        extract_dir(&fixture("go/basic"), "tiny-go", Some("go")).expect("extract should succeed");

    let all = names(&g.nodes);

    assert!(all.contains(&"User"), "missing struct User — got {all:?}");
    assert!(all.contains(&"Greeter"), "missing interface Greeter");
    assert!(all.contains(&"Handler"), "missing struct Handler");
    assert!(all.contains(&"NewHandler"), "missing func NewHandler");
    assert!(all.contains(&"Greet"), "missing method Greet");
    assert!(all.contains(&"Handle"), "missing method Handle");

    let files = names_of_kind(&g.nodes, "file");
    assert!(files.contains(&"models.go"));
    assert!(files.contains(&"handlers.go"));
}

#[test]
fn go_adversarial_does_not_panic() {
    let g = extract_dir(&fixture("go/adversarial"), "go-adversarial", Some("go"))
        .expect("extract should not error");

    let files = names_of_kind(&g.nodes, "file");
    for expected in ["malformed.go", "empty.go", "bom.go", "crlf.go"] {
        assert!(
            files.contains(&expected),
            "expected file node for {expected} — got {files:?}"
        );
    }

    let all = names(&g.nodes);
    assert!(
        all.contains(&"WithCrlf"),
        "CRLF .go should still extract — got {all:?}"
    );
    assert!(
        all.contains(&"WithBom"),
        "BOM .go should still extract — got {all:?}"
    );
}

// ─── C++: header + implementation ──────────────────────────────────

#[test]
fn cpp_basic_extracts_classes_methods_namespace() {
    let g = extract_dir(&fixture("cpp/basic"), "tiny-cpp", Some("cpp"))
        .expect("extract should succeed");

    let all = names(&g.nodes);

    assert!(all.contains(&"Node"), "missing class Node — got {all:?}");
    assert!(all.contains(&"Edge"), "missing class Edge");
    assert!(
        all.contains(&"Container"),
        "missing template class Container"
    );
    assert!(all.contains(&"main"), "missing main function");

    let files = names_of_kind(&g.nodes, "file");
    assert!(files.contains(&"graph.h"));
    assert!(files.contains(&"graph.cpp"));
    assert!(files.contains(&"main.cpp"));
}

#[test]
fn cpp_basic_extracts_inherits_edge() {
    let g = extract_dir(&fixture("cpp/basic"), "tiny-cpp", Some("cpp"))
        .expect("extract should succeed");

    let inherits = edges_of_kind(&g.edges, "inherits");
    let id_name = id_to_name(&g.nodes);

    // class Edge : public Node — Edge inherits Node
    let has_edge_inherits = inherits.iter().any(|e| {
        let from = id_name.get(e.from_id.as_str()).copied().unwrap_or("");
        let to = id_name.get(e.to_id.as_str()).copied().unwrap_or("");
        from == "Edge" && to == "Node"
    });
    assert!(
        has_edge_inherits,
        "expected Edge inherits Node — got {} inherits edges",
        inherits.len()
    );
}

#[test]
fn cpp_adversarial_does_not_panic() {
    let g = extract_dir(&fixture("cpp/adversarial"), "cpp-adversarial", Some("cpp"))
        .expect("extract should not error");

    let files = names_of_kind(&g.nodes, "file");
    for expected in ["malformed.cpp", "empty.cpp", "bom.cpp", "crlf.cpp"] {
        assert!(
            files.contains(&expected),
            "expected file node for {expected} — got {files:?}"
        );
    }

    let all = names(&g.nodes);
    assert!(all.contains(&"with_crlf"), "CRLF .cpp should still extract");
    assert!(all.contains(&"with_bom"), "BOM .cpp should still extract");
}

// ─── Markdown ──────────────────────────────────────────────────────

#[test]
fn markdown_basic_extracts_doc_sections() {
    // Markdown doesn't need a language hint; walk_dir handles .md specially.
    let g =
        extract_dir(&fixture("markdown/basic"), "tiny-md", None).expect("extract should succeed");

    let all = names(&g.nodes);
    let files = names_of_kind(&g.nodes, "file");

    // We expect at least a file node and some doc section nodes.
    assert!(files.contains(&"README.md"), "expected README.md file node");
    // Section names from the markdown headings.
    let saw_section = all.iter().any(|n| {
        n.contains("Overview")
            || n.contains("Quick start")
            || n.contains("API")
            || n.contains("Errors")
    });
    assert!(
        saw_section,
        "expected at least one heading-derived doc section — got {all:?}"
    );
}

#[test]
fn markdown_adversarial_does_not_panic() {
    let g = extract_dir(&fixture("markdown/adversarial"), "md-adversarial", None)
        .expect("extract should not error on adversarial markdown");

    let files = names_of_kind(&g.nodes, "file");
    for expected in ["uneven_headings.md", "unclosed_codeblock.md", "empty.md"] {
        assert!(
            files.contains(&expected),
            "expected file node for {expected} — got {files:?}"
        );
    }
}
