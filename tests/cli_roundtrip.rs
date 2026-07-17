//! End-to-end CLI round-trips over the real `roux` binary (`CARGO_BIN_EXE_roux`):
//! `add` -> `query` returns the indexed symbol, and the status contract holds
//! (stdout stays pure data; `--quiet`/`--verbose` gate stderr). Every command
//! runs with `HOME`/`XDG_*` redirected into the test's temp dir and uses
//! `--local`, so no test can read or write the developer's real global store.

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

/// Run `roux` with `args` in `cwd`, with HOME/XDG isolated under `cwd` so the
/// global store can never resolve into the real environment. Returns
/// `(success, stdout, stderr)`.
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

/// Write `dir/src/lib.rs` with `body` under `cwd` and index it locally as `name`.
fn seed(cwd: &Path, dir: &str, name: &str, body: &str) {
    let src = cwd.join(dir).join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("lib.rs"), body).unwrap();
    let (ok, out, err) = run(
        cwd,
        &["add", dir, "--local", "--lang", "rust", "--name", name],
    );
    assert!(ok, "`roux add {dir}` failed: stdout={out:?} stderr={err:?}");
}

#[test]
fn add_then_query_returns_the_indexed_symbol() {
    let tmp = tempfile::tempdir().unwrap();
    seed(
        tmp.path(),
        "proj",
        "proj",
        "pub fn compute_shipping_cost() {}\n",
    );

    let (ok, stdout, err) = run(tmp.path(), &["query", "compute_shipping_cost", "--local"]);
    assert!(ok, "query failed: {err:?}");
    assert!(
        stdout.contains("compute_shipping_cost"),
        "query stdout should name the matched symbol, got {stdout:?}"
    );
}

#[test]
fn query_json_stdout_is_pure_data() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path(), "proj", "proj", "pub fn alpha() {}\n");

    let (ok, stdout, err) = run(
        tmp.path(),
        &["query", "alpha", "--local", "--format", "json"],
    );
    assert!(ok);
    let v: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not valid JSON ({e}): {stdout:?}"));
    assert!(
        err.is_empty(),
        "normal JSON query must keep stderr clean, got {err:?}"
    );
    assert!(
        v.get("symbols").is_some(),
        "JSON envelope must have symbols"
    );
    // stdout is data: no ANSI color and no status prefixes/branding leak in.
    assert!(
        !stdout.contains('\u{1b}'),
        "stdout must carry no ANSI escapes"
    );
    assert!(
        !stdout.contains('\u{2756}') && !stdout.contains("warn "),
        "stdout must carry no status branding, got {stdout:?}"
    );
}

#[test]
fn quiet_silences_routine_status_but_keeps_stdout_data() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path(), "proj", "proj", "pub fn alpha() {}\n");

    let (ok, stdout, stderr) = run(
        tmp.path(),
        &["--quiet", "query", "alpha", "--local", "--format", "json"],
    );
    assert!(ok);
    assert!(
        stderr.is_empty(),
        "--quiet must silence routine stderr on success, got {stderr:?}"
    );
    assert!(
        serde_json::from_str::<Value>(&stdout).is_ok(),
        "stdout data must survive --quiet, got {stdout:?}"
    );
}

#[test]
fn verbose_emits_detail_that_normal_hides() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path(), "proj", "proj", "pub fn alpha() {}\n");

    let (normal_ok, _out, normal_err) = run(tmp.path(), &["query", "alpha", "--local"]);
    let (verbose_ok, _out2, verbose_err) =
        run(tmp.path(), &["--verbose", "query", "alpha", "--local"]);
    assert!(normal_ok && verbose_ok, "both queries must succeed");
    assert!(
        !normal_err.contains("matched"),
        "normal run must not emit --verbose detail, got {normal_err:?}"
    );
    assert!(
        verbose_err.contains("matched"),
        "--verbose must emit match-count detail, got {verbose_err:?}"
    );
}

#[test]
fn multi_source_add_makes_every_source_queryable() {
    let tmp = tempfile::tempdir().unwrap();
    for (dir, sym) in [("a", "fn_from_a"), ("b", "fn_from_b")] {
        let src = tmp.path().join(dir).join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("lib.rs"), format!("pub fn {sym}() {{}}\n")).unwrap();
    }
    let (ok, out, err) = run(tmp.path(), &["add", "a", "b", "--local", "--lang", "rust"]);
    assert!(ok, "multi-source add failed: stdout={out:?} stderr={err:?}");

    for sym in ["fn_from_a", "fn_from_b"] {
        let (ok, stdout, _e) = run(tmp.path(), &["query", sym, "--local"]);
        assert!(
            ok && stdout.contains(sym),
            "expected `{sym}` to be findable, got {stdout:?}"
        );
    }
}

#[test]
fn remove_drops_the_source_from_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    seed(tmp.path(), "proj", "proj", "pub fn alpha() {}\n");

    let (_ok, before, _e) = run(tmp.path(), &["list", "--local"]);
    assert!(
        before.contains("proj"),
        "list should show the source: {before:?}"
    );

    let (ok, _o, err) = run(tmp.path(), &["remove", "proj"]);
    assert!(ok, "remove failed: {err:?}");

    let (_ok2, after, _e2) = run(tmp.path(), &["list", "--local"]);
    assert!(
        !after.contains("proj"),
        "removed source must be gone from list, got {after:?}"
    );
}

#[test]
fn completions_generate_a_usable_script() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, stdout, err) = run(tmp.path(), &["completions", "bash"]);
    assert!(ok, "completions failed: {err:?}");
    assert!(
        stdout.contains("_roux"),
        "bash completion should define the _roux function, got {stdout:?}"
    );
}
