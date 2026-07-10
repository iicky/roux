//! Error-path and edge-case tests across modules.
//!
//! Existing unit tests cover happy paths. These tests pin down behavior
//! on bad input — corrupted files, weird queries, missing data — so that
//! "we degrade gracefully" is a guarantee rather than a hope.

use std::fs;

use roux_cli::config::Config;
use roux_cli::graph::store::GraphStore;
use roux_cli::lockfile;
use roux_cli::source::Source;

// ─── Store: open / migrate ─────────────────────────────────────────

#[test]
fn store_open_creates_missing_parent_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("a/b/c/db.sqlite");
    assert!(!nested.parent().unwrap().exists());

    let store = GraphStore::open(&nested);
    assert!(
        store.is_ok(),
        "open should auto-create parent dirs — got {:?}",
        store.err()
    );
    assert!(nested.parent().unwrap().exists());
    assert!(nested.exists());
}

#[test]
fn store_open_corrupted_file_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt.sqlite");
    // Write a non-SQLite file. SQLite open() itself succeeds (it'll treat
    // it as a fresh DB on first write), but migrate() should choke when it
    // tries to read schema_version from a nonsense file.
    fs::write(
        &path,
        b"this is not a sqlite database, just garbage bytes\n",
    )
    .unwrap();

    let result = GraphStore::open(&path);
    assert!(
        result.is_err(),
        "expected error opening a corrupted file, got Ok"
    );
}

// ─── Store: search edge cases ──────────────────────────────────────

#[test]
fn store_search_empty_query_does_not_panic() {
    let store = GraphStore::open_in_memory().unwrap();
    // Empty query string after FTS escape produces an empty MATCH expression.
    // Whether this returns Ok([]) or Err is fine; what matters is no panic.
    let _ = store.search("", 10);
}

#[test]
fn store_search_only_punctuation_does_not_panic() {
    let store = GraphStore::open_in_memory().unwrap();
    // FTS escape strips non-alphanumeric, so this collapses to empty.
    let _ = store.search("!@#$%^&*()", 10);
    let _ = store.search("\"\"\"", 10);
    let _ = store.search("- - -", 10);
}

#[test]
fn store_search_unicode_query_does_not_panic() {
    // Query-side stemming strips a doubled trailing consonant before "ed".
    // The doubled-char check is by `char` but must not slice by byte, or a
    // multibyte trailing char panics ("not a char boundary"). Regression for
    // the `roux query "ééed"` crash.
    let store = GraphStore::open_in_memory().unwrap();
    for q in ["ééed", "naïveed", "über", "ﬀed", "日本ed", "café"] {
        let result = store.search(q, 10);
        assert!(
            result.is_ok(),
            "search({q:?}) should not panic and should be Ok — got {:?}",
            result.err()
        );
    }
}

#[test]
fn store_search_fts_special_chars_does_not_panic() {
    // FTS5 has its own query syntax (MATCH, AND, OR, NEAR, "...", phrase
    // queries). Raw user input must be escaped or the parser will reject it.
    // fts_query_escape() in store.rs is responsible for this; the test pins
    // the contract.
    let store = GraphStore::open_in_memory().unwrap();
    for q in [
        "foo AND bar",
        "foo OR bar NEAR baz",
        "foo \" bar",
        "(foo bar)",
        "* foo *",
        "foo: bar",
    ] {
        let result = store.search(q, 10);
        assert!(
            result.is_ok(),
            "search({q:?}) should be Ok (escape strips special chars) — got {:?}",
            result.err()
        );
    }
}

#[test]
fn store_search_zero_limit_returns_empty() {
    let store = GraphStore::open_in_memory().unwrap();
    let result = store.search("anything", 0).unwrap();
    assert!(
        result.nodes.is_empty(),
        "expected zero results with limit=0, got {}",
        result.nodes.len()
    );
}

#[test]
fn store_search_on_empty_db_returns_empty() {
    let store = GraphStore::open_in_memory().unwrap();
    let result = store.search("nonexistent symbol", 10).unwrap();
    assert!(
        result.nodes.is_empty(),
        "expected zero results on empty db, got {}",
        result.nodes.len()
    );
}

// ─── Store: upsert idempotency ─────────────────────────────────────

#[test]
fn store_upsert_idempotent_for_same_source() {
    use roux_cli::graph::Node;

    let store = GraphStore::open_in_memory().unwrap();

    let make_node = |name: &str| Node {
        id: Node::id_for("src", &format!("src::{name}")),
        kind: "function".into(),
        name: name.into(),
        qualified_name: format!("src::{name}"),
        source_name: "src".into(),
        language: "rust".into(),
        file_path: "lib.rs".into(),
        start_line: 1,
        start_col: 0,
        end_line: 5,
        visibility: "pub".into(),
        signature: Some(format!("fn {name}()")),
        doc: None,
        body: format!("function: src::{name}"),
        parent_id: None,
        content_hash: None,
        line_count: 5,
        source_url: None,
        description: None,
    };
    let nodes = vec![make_node("alpha"), make_node("beta")];
    let edges = Vec::new();

    store
        .upsert_source("src", "v1", "rust", &nodes, &edges)
        .unwrap();
    let first = store.search("alpha", 10).unwrap().nodes.len();

    // Upsert the same source again — should not duplicate.
    store
        .upsert_source("src", "v1", "rust", &nodes, &edges)
        .unwrap();
    let second = store.search("alpha", 10).unwrap().nodes.len();

    assert_eq!(
        first, second,
        "upsert should be idempotent — {first} hits before, {second} after"
    );
}

// ─── Store: cross-language separator handling ─────────────────────

#[test]
fn store_search_cross_language_separators() {
    use roux_cli::graph::Node;

    // Symbols indexed with different language conventions:
    //   Rust:    tokio::task::spawn
    //   Python:  tokio.task.spawn
    // A user (or agent) querying with either separator — or just spaces —
    // should match both. Pre-fix, "tokio::spawn" would collapse to a single
    // "tokiospawn" token at query time and miss every node in the index.
    let make_node = |source: &str, qualified: &str, lang: &str| Node {
        id: Node::id_for(source, qualified),
        kind: "function".into(),
        name: "spawn".into(),
        qualified_name: qualified.into(),
        source_name: source.into(),
        language: lang.into(),
        file_path: "lib".into(),
        start_line: 1,
        start_col: 0,
        end_line: 5,
        visibility: "pub".into(),
        signature: Some("fn spawn()".into()),
        doc: None,
        body: format!("function: {qualified}"),
        parent_id: None,
        content_hash: None,
        line_count: 5,
        source_url: None,
        description: None,
    };

    let store = GraphStore::open_in_memory().unwrap();
    store
        .upsert_source(
            "tokio-rs",
            "v1",
            "rust",
            &[make_node("tokio-rs", "tokio::task::spawn", "rust")],
            &[],
        )
        .unwrap();
    store
        .upsert_source(
            "tokio-py",
            "v1",
            "python",
            &[make_node("tokio-py", "tokio.task.spawn", "python")],
            &[],
        )
        .unwrap();

    for q in ["tokio spawn", "tokio::spawn", "tokio.spawn", "task spawn"] {
        let result = store.search(q, 10).unwrap();
        assert_eq!(
            result.nodes.len(),
            2,
            "search({q:?}) should match both rust and python tokens, got {}",
            result.nodes.len()
        );
    }
}

// ─── Lockfile: malformed input ─────────────────────────────────────

#[test]
fn lockfile_detect_project_no_lockfiles_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let result = lockfile::detect_project(tmp.path());
    assert!(
        result.is_none(),
        "empty dir should return None, got {result:?}"
    );
}

#[test]
fn lockfile_detect_project_corrupted_cargo_lock_warns_and_continues() {
    // Existing behavior in src/lockfile.rs:73 is to eprintln and `continue` on
    // parser failure, which means detect_project returns None rather than
    // erroring. This test pins that contract — corrupted lockfiles should
    // never panic the CLI or surface a hard error.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("Cargo.lock"), b"this is not valid TOML {{").unwrap();

    let result = lockfile::detect_project(tmp.path());
    assert!(
        result.is_none(),
        "corrupted Cargo.lock should degrade to None, got {result:?}"
    );
}

#[test]
fn lockfile_detect_project_empty_cargo_lock_returns_some_with_no_deps() {
    // An empty TOML is valid; it just has no [[package]] entries.
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("Cargo.lock"), b"").unwrap();

    let result = lockfile::detect_project(tmp.path()).expect("empty Cargo.lock should parse");
    assert_eq!(result.kind, lockfile::ProjectKind::Rust);
    assert!(
        result.deps.is_empty(),
        "expected no dependencies from empty lockfile"
    );
}

#[test]
fn lockfile_detect_project_corrupted_package_lock_warns_and_continues() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(
        tmp.path().join("package-lock.json"),
        b"{ this is not valid json,",
    )
    .unwrap();

    let result = lockfile::detect_project(tmp.path());
    assert!(
        result.is_none(),
        "corrupted package-lock.json should degrade to None"
    );
}

#[test]
fn lockfile_detect_project_skips_directories_named_like_lockfiles() {
    // Edge case: a directory that happens to be named Cargo.lock should not
    // be treated as a lockfile. Detect should fall through to other candidates
    // or return None.
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join("Cargo.lock")).unwrap();

    let result = lockfile::detect_project(tmp.path());
    // It's fine if this returns None or hits some other lockfile; what matters
    // is it doesn't panic and doesn't claim to have parsed a directory.
    if let Some(info) = result {
        assert!(
            info.lockfile.is_file(),
            "detect_project returned a directory as a lockfile path: {:?}",
            info.lockfile
        );
    }
}

// ─── Config: parsing bad input ─────────────────────────────────────

#[test]
fn config_parse_malformed_toml_errors() {
    // Syntactically broken TOML must surface an error, not panic or silently
    // hand back defaults (which would mask a typo'd config).
    let result = Config::parse("this is = = not valid toml [[[");
    assert!(result.is_err(), "malformed TOML should error");
}

#[test]
fn config_parse_wrong_type_errors() {
    // A known key with the wrong value type is a user mistake worth surfacing.
    let result = Config::parse("[index]\nprefer_local = \"lots\"");
    assert!(
        result.is_err(),
        "a string where a number is expected should error"
    );
}

#[test]
fn config_parse_empty_and_unknown_keys_are_lenient() {
    // Empty config → defaults. Unknown keys are ignored (forward-compatible),
    // not fatal — so an older binary tolerates a newer config.
    assert!(Config::parse("").is_ok());
    assert!(
        Config::parse("[index]\nprefer_local = true\nfuture_knob = 9").is_ok(),
        "unknown keys should be tolerated"
    );
}

// ─── Source: name / kind inference on odd input ────────────────────

#[test]
fn source_from_raw_nonexistent_path_is_treated_as_crate() {
    // A bare token that isn't an existing path is assumed to be a crate name —
    // it must not panic or try to read the filesystem as if it existed.
    let src = Source::from_raw("definitely-not-a-real-path-xyz", None, None, None);
    assert_eq!(src.name, "definitely-not-a-real-path-xyz");
}

#[test]
fn source_from_raw_local_dir_name_from_basename() {
    let tmp = tempfile::tempdir().unwrap();
    let sub = tmp.path().join("my_proj");
    fs::create_dir(&sub).unwrap();
    let src = Source::from_raw(sub.to_str().unwrap(), None, None, None);
    assert_eq!(src.name, "my_proj", "name should derive from dir basename");
}

#[test]
fn source_detected_language_unknown_extension_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("data.xyzzy");
    fs::write(&f, "blob").unwrap();
    let src = Source::from_raw(f.to_str().unwrap(), None, None, None);
    // Unknown extension → no detected language, rather than a wrong guess.
    assert!(src.detected_language().is_none());
}

#[test]
fn source_explicit_language_overrides_detection() {
    let src = Source::from_raw("whatever.py", None, Some("rust".into()), None);
    assert_eq!(src.detected_language(), Some("rust"));
}

// ─── Artifact: compatibility refusal paths ─────────────────────────

#[test]
fn artifact_check_rejects_plain_index() {
    // A normal index DB has no `artifact_version` manifest row, so querying it
    // as a downloadable artifact must be refused with a clear message — not
    // accepted as if it were a published artifact.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("db.sqlite");
    let _ = GraphStore::open(&db).unwrap(); // creates a plain index, no manifest

    let err = roux_cli::artifact::check_artifact_compatibility(&db)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("not a roux artifact"),
        "expected a clear non-artifact message, got: {err}"
    );
}

#[test]
fn artifact_check_missing_file_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("nope.sqlite");
    assert!(roux_cli::artifact::check_artifact_compatibility(&missing).is_err());
}

// ─── Extraction: empty / unsupported sources ───────────────────────

#[test]
fn extract_dir_empty_directory_yields_no_symbols() {
    let tmp = tempfile::tempdir().unwrap();
    let g = roux_cli::graph::extract::extract_dir(tmp.path(), "empty", "0", Some("rust")).unwrap();
    assert!(g.nodes.is_empty(), "empty dir should produce no symbols");
}

#[test]
fn extract_dir_only_unsupported_files_yields_no_code_symbols() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("notes.bin"), [0u8, 1, 2, 255]).unwrap();
    fs::write(tmp.path().join("data.unknownext"), "nothing parseable").unwrap();
    // Must not panic on binary/unknown content; produces no code symbols.
    let g = roux_cli::graph::extract::extract_dir(tmp.path(), "weird", "0", None).unwrap();
    assert!(
        g.nodes.iter().all(|n| n.kind != "function" && n.kind != "class"),
        "unsupported files should not yield code symbols"
    );
}
