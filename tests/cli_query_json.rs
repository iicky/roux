//! Regression test for the zero-result JSON contract: `roux query --format json` must emit a
//! well-formed JSON envelope (not empty stdout) on a zero-match query, so
//! programmatic consumers (agents, MCP, scripts) don't choke on empty
//! results.
//!
//! Spawns the real `roux` binary (via `CARGO_BIN_EXE_roux`) against a small
//! local index built from a real Python file, so the assertions exercise the
//! full CLI parse -> query -> format pipeline, not just the JSON serializer.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Run the `roux` binary with `args` in `cwd`, returning
/// `(success, stdout, stderr)` decoded as UTF-8.
fn run(cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_roux"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `roux {args:?}`: {e}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Build a local index (`<tmp>/.roux/db.sqlite`) from one real Python
/// function, for the `roux query` scenarios below to search against.
fn seed_local_index(tmp: &Path) {
    let pkg = tmp.join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("orders.py"),
        "def compute_order_total(items):\n    return sum(i.price for i in items)\n",
    )
    .unwrap();

    let (ok, stdout, stderr) = run(
        tmp,
        &["add", ".", "--local", "--lang", "python", "--name", "t"],
    );
    assert!(ok, "`roux add` failed: stdout={stdout:?} stderr={stderr:?}");
}

#[test]
fn query_json_zero_match_emits_well_formed_envelope() {
    let tmp = tempfile::tempdir().unwrap();
    seed_local_index(tmp.path());

    // The regression itself: a query with zero matches must still emit a
    // well-formed JSON envelope on stdout, not empty output. Before
    // this fix, `cmd_query`'s early "No results found." return fired
    // for every format including json, leaving stdout empty and breaking any
    // consumer that expects to parse valid JSON.
    let (ok, stdout, stderr) = run(
        tmp.path(),
        &[
            "query",
            "zzznomatchxyz",
            "--local",
            "--format",
            "json",
            "--top",
            "5",
        ],
    );
    assert!(ok, "query should exit 0 on zero matches: stderr={stderr:?}");
    assert!(
        stderr.trim().is_empty(),
        "json format should not print anything to stderr, got {stderr:?}"
    );
    assert!(
        !stdout.trim().is_empty(),
        "stdout must not be empty on a zero-match JSON query — the zero-result JSON bug \
         (empty stdout instead of a JSON envelope)"
    );

    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout did not parse as JSON: {e}\nstdout={stdout:?}"));
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("expected a JSON object envelope, got {value}"));

    for key in ["symbols", "edges", "matched"] {
        let arr = obj
            .get(key)
            .unwrap_or_else(|| panic!("missing `{key}` key in JSON envelope: {value}"))
            .as_array()
            .unwrap_or_else(|| panic!("`{key}` should be a JSON array, got {value}"));
        assert!(
            arr.is_empty(),
            "`{key}` should be an empty array on a zero-match query, got {arr:?}"
        );
    }
}

#[test]
fn query_json_real_match_yields_non_empty_symbols() {
    // Positive control, guarding against a vacuous pass above: if this
    // failed to find a real match, the empty-arrays assertions in the
    // zero-match test could pass even from a query pipeline that always
    // returns nothing.
    let tmp = tempfile::tempdir().unwrap();
    seed_local_index(tmp.path());

    let (ok, stdout, stderr) = run(
        tmp.path(),
        &[
            "query",
            "compute order total",
            "--local",
            "--format",
            "json",
            "--top",
            "3",
        ],
    );
    assert!(ok, "query should exit 0: stderr={stderr:?}");

    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout did not parse as JSON: {e}\nstdout={stdout:?}"));
    let symbols = value["symbols"]
        .as_array()
        .unwrap_or_else(|| panic!("`symbols` should be a JSON array, got {value}"));
    assert!(
        !symbols.is_empty(),
        "expected a real match for `compute order total` against compute_order_total, \
         got empty symbols: {value}"
    );
}

#[test]
fn query_text_zero_match_prints_no_results_message() {
    // Default (text) format keeps its pre-fix behavior on zero matches:
    // empty stdout, a human-readable message on stderr. This pins down that
    // the json-only carve-out did not regress the human-readable path.
    let tmp = tempfile::tempdir().unwrap();
    seed_local_index(tmp.path());

    let (ok, stdout, stderr) = run(
        tmp.path(),
        &["query", "zzznomatchxyz", "--local", "--top", "5"],
    );
    assert!(ok, "query should exit 0 on zero matches: stderr={stderr:?}");
    assert!(
        stdout.trim().is_empty(),
        "text format should print nothing to stdout on zero matches, got {stdout:?}"
    );
    assert!(
        stderr.contains("No results found."),
        "expected 'No results found.' on stderr, got {stderr:?}"
    );
}
