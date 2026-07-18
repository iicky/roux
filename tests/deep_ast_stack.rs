//! Regression: `roux init` stack-overflowed while ingesting a
//! dependency because crate extraction ran on a worker thread with the default
//! (~2 MB) stack, while the deep recursive AST walk needs the same large stack
//! `main` reserves for local extraction. This exercises that path on deeply
//! nested Rust source and asserts extraction completes without overflowing.

use std::thread;

use roux_cli::graph::extract::extract_dir;

/// Build a Rust source file with `depth` levels of nested blocks + calls — the
/// shape that drives the recursive-descent extractors (`extract_calls_recursive`,
/// `extract_node`, …) as deep as the syntax nests.
fn deeply_nested_rust(depth: usize) -> String {
    let mut body = String::from("let x = 0;");
    for _ in 0..depth {
        body = format!("{{ f(); {body} }}");
    }
    format!("fn f() {{}}\nfn deep() {{\n{body}\n}}\n")
}

fn extract_deep_on_stack(stack_bytes: usize, depth: usize) -> bool {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("deep.rs"), deeply_nested_rust(depth)).unwrap();
    let path = dir.path().to_path_buf();

    thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(move || extract_dir(&path, "deep", Some("rust")).is_ok())
        .unwrap()
        .join()
        .expect("extraction thread must not abort")
}

#[test]
fn deep_ast_extracts_on_worker_sized_stack() {
    // 20k-deep nesting overflows the default ~2 MB thread stack around 5k levels
    // (the reported crash) but fits the 64 MB stack the worker now uses. If a
    // future change shrinks the worker stack, this test aborts — the regression.
    let stack = roux_cli::settings::get().worker_stack_bytes;
    assert!(
        extract_deep_on_stack(stack, 20_000),
        "deep AST extraction should succeed on the worker-sized stack"
    );
}
