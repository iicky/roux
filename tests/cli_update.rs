//! End-to-end coverage for `roux update`: the incremental-refresh command
//! that diffs a path source's working tree against the stored manifest,
//! re-extracts only changed files, and applies the row delta. The
//! correctness core (DB-prior reconstruction composed with
//! `reextract_incremental` and `apply_source_delta`) is proven at the unit
//! level in `graph::store`'s test module; these tests drive the real binary
//! to prove the CLI wiring on top of it: that an update's effects are
//! visible to subsequent queries, that a no-op refresh is idempotent and
//! content-aware (not mtime-based), and that `--dry-run` applies nothing.

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
        .env("HOME", cwd)
        .env("XDG_DATA_HOME", cwd.join("xdg-data"))
        .env("XDG_CONFIG_HOME", cwd.join("xdg-config"))
        .env("ROUX_GLOBAL_PATH", cwd.join("global").join("db.sqlite"))
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `roux {args:?}`: {e}"));
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Write a two-file Rust project where `caller.rs` calls a fn defined in
/// `callee.rs` — a cross-file reference `roux update` must correctly
/// re-resolve when `callee_body` changes the callee's shape.
fn write_project(tmp: &Path, callee_body: &str) {
    let src = tmp.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("caller.rs"),
        "pub fn invoke_thing() { do_the_thing(); }\n",
    )
    .unwrap();
    fs::write(src.join("callee.rs"), callee_body).unwrap();
}

fn add_smoke_source(tmp: &Path) {
    let (ok, stdout, stderr) = run(
        tmp,
        &["add", ".", "--local", "--lang", "rust", "--name", "smoke"],
    );
    assert!(ok, "`roux add` failed: stdout={stdout:?} stderr={stderr:?}");
}

/// `qualified_name` of every symbol in a `roux query --format json` response.
fn query_qualified_names(tmp: &Path, query: &str) -> Vec<String> {
    let (ok, stdout, stderr) = run(
        tmp,
        &["query", query, "--local", "--format", "json", "--top", "10"],
    );
    assert!(
        ok,
        "`roux query` failed: stdout={stdout:?} stderr={stderr:?}"
    );
    let value: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout did not parse as JSON: {e}\nstdout={stdout:?}"));
    value["symbols"]
        .as_array()
        .unwrap_or_else(|| panic!("`symbols` should be a JSON array, got {value}"))
        .iter()
        .map(|s| {
            s["qualified_name"]
                .as_str()
                .unwrap_or_else(|| panic!("symbol missing `qualified_name`: {s}"))
                .to_string()
        })
        .collect()
}

#[test]
fn update_reflects_rename_and_addition_in_subsequent_queries() {
    let tmp = tempfile::tempdir().unwrap();
    write_project(tmp.path(), "pub fn do_the_thing() {}\n");
    add_smoke_source(tmp.path());

    // Rename the callee across the caller boundary (caller.rs is left
    // untouched, so its call site now points at a name that no longer
    // exists) and add a brand-new fn.
    write_project(
        tmp.path(),
        "pub fn do_the_thing_v2() {}\npub fn brand_new_capability() {}\n",
    );

    let (ok, stdout, stderr) = run(tmp.path(), &["update", "--local"]);
    assert!(ok, "update failed: stdout={stdout:?} stderr={stderr:?}");

    let new_names = query_qualified_names(tmp.path(), "brand_new_capability");
    assert!(
        new_names.iter().any(|q| q == "smoke::brand_new_capability"),
        "expected the newly added fn to be queryable after update, got {new_names:?}"
    );

    let old_names = query_qualified_names(tmp.path(), "do_the_thing");
    assert!(
        !old_names.iter().any(|q| q == "smoke::do_the_thing"),
        "the renamed fn's old qualified_name must not survive the update, got {old_names:?}"
    );
}

#[test]
fn update_with_no_edit_is_idempotent_and_reports_up_to_date() {
    let tmp = tempfile::tempdir().unwrap();
    write_project(tmp.path(), "pub fn do_the_thing() {}\n");
    add_smoke_source(tmp.path());

    let (ok, stdout, stderr) = run(tmp.path(), &["update", "--local"]);
    assert!(ok, "update failed: stdout={stdout:?} stderr={stderr:?}");
    assert!(
        stderr.contains("up to date"),
        "an unedited source should report up to date, got stderr={stderr:?}"
    );
    assert!(
        stderr.contains("0 updated, 1 up to date"),
        "a no-op refresh should update 0 sources, got stderr={stderr:?}"
    );
}

#[test]
fn update_after_content_preserving_touch_reports_up_to_date() {
    // Rewriting a file with byte-identical content (a "touch") must not
    // register as a change: the diff is content-hash based, not mtime based.
    let tmp = tempfile::tempdir().unwrap();
    write_project(tmp.path(), "pub fn do_the_thing() {}\n");
    add_smoke_source(tmp.path());

    let callee = tmp.path().join("src/callee.rs");
    let bytes = fs::read(&callee).unwrap();
    fs::write(&callee, &bytes).unwrap();

    let (ok, stdout, stderr) = run(tmp.path(), &["update", "--local"]);
    assert!(ok, "update failed: stdout={stdout:?} stderr={stderr:?}");
    assert!(
        stderr.contains("0 updated, 1 up to date"),
        "a byte-identical rewrite (mtime-only touch) should still report up to date, \
         got stderr={stderr:?}"
    );
}

#[test]
fn dry_run_reports_would_update_but_applies_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    write_project(tmp.path(), "pub fn do_the_thing() {}\n");
    add_smoke_source(tmp.path());

    write_project(
        tmp.path(),
        "pub fn do_the_thing_v2() {}\npub fn brand_new_capability() {}\n",
    );

    let (ok, stdout, stderr) = run(tmp.path(), &["update", "--local", "--dry-run"]);
    assert!(
        ok,
        "dry-run update failed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("would update"),
        "dry-run should report it would update without applying, got stderr={stderr:?}"
    );

    // Dry run must apply nothing: queries still reflect the pre-edit state.
    let pre_edit_old = query_qualified_names(tmp.path(), "do_the_thing");
    assert!(
        pre_edit_old.iter().any(|q| q == "smoke::do_the_thing"),
        "dry-run must not touch the index — the old symbol should still be queryable, \
         got {pre_edit_old:?}"
    );
    let pre_edit_new = query_qualified_names(tmp.path(), "brand_new_capability");
    assert!(
        !pre_edit_new
            .iter()
            .any(|q| q == "smoke::brand_new_capability"),
        "dry-run must not apply the change — the new symbol should not be queryable yet, \
         got {pre_edit_new:?}"
    );

    // A real update now applies the change.
    let (ok, stdout, stderr) = run(tmp.path(), &["update", "--local"]);
    assert!(ok, "update failed: stdout={stdout:?} stderr={stderr:?}");

    let post_edit_new = query_qualified_names(tmp.path(), "brand_new_capability");
    assert!(
        post_edit_new
            .iter()
            .any(|q| q == "smoke::brand_new_capability"),
        "a real update following the dry-run should apply the change, got {post_edit_new:?}"
    );
}
