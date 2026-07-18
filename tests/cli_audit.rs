//! End-to-end tests for `roux audit` driving the real binary. Audit extracts a
//! tree fresh (no pre-built index), so these craft a source dir and assert the
//! classifier's verdicts survive the full CLI path.

use std::path::Path;
use std::process::Command;

fn run(cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_roux"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("XDG_DATA_HOME", cwd)
        .env("XDG_CONFIG_HOME", cwd)
        .env("ROUX_GLOBAL_PATH", cwd.join("global.sqlite"))
        .output()
        .expect("spawn roux");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn audit_json_flags_a_collision_and_spares_a_clean_symbol() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    // Nine same-named `parse` methods bury each other (a collision); a
    // distinctive, documented, referenced function should NOT be flagged.
    std::fs::write(
        src.join("lib.rs"),
        r#"
        pub struct Config; impl Config { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct A; impl A { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct B; impl B { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct C; impl C { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct D; impl D { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct E; impl E { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct F; impl F { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct G; impl G { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct H; impl H { pub fn parse(s: &str) -> u8 { 0 } }

        /// Compute the blake3 fingerprint of a directory tree for staleness.
        pub fn fingerprint_directory_tree() -> u64 { seed() }
        fn seed() -> u64 { 0 }
        "#,
    )
    .unwrap();

    let (ok, stdout, stderr) = run(
        tmp.path(),
        &["audit", src.to_str().unwrap(), "--format", "json"],
    );
    assert!(ok, "audit should exit 0: stderr={stderr}");

    let v: serde_json::Value =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout not JSON: {e}\n{stdout}"));
    assert!(
        v["audited"].as_u64().unwrap() >= 9,
        "should audit the public symbols: {stdout}"
    );
    let findings = v["findings"].as_array().expect("findings array");

    // At least one same-named `parse` sibling is buried -> classified collision,
    // and its fix is disambiguation, never "add a doc".
    let parse_collision = findings
        .iter()
        .find(|f| f["cause"] == "collision" && f["symbol"].as_str().unwrap().ends_with("::parse"));
    let f = parse_collision
        .unwrap_or_else(|| panic!("expected a colliding `parse` finding in {stdout}"));
    assert!(
        f["fix"].as_str().unwrap().contains("rename")
            || f["fix"].as_str().unwrap().contains("distinctive"),
        "collision fix must be disambiguation: {f}"
    );

    // The distinctive, documented, connected symbol must not be a finding.
    assert!(
        !findings.iter().any(|f| f["symbol"]
            .as_str()
            .unwrap()
            .ends_with("fingerprint_directory_tree")),
        "a clean symbol must not be flagged: {stdout}"
    );
}

#[test]
fn audit_rejects_unknown_format() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, _stdout, stderr) = run(tmp.path(), &["audit", ".", "--format", "yaml"]);
    assert!(!ok, "an invalid --format must fail: {stderr}");
    assert!(
        stderr.contains("yaml")
            || stderr.to_lowercase().contains("invalid")
            || stderr.contains("possible values"),
        "clap should reject the format value: {stderr}"
    );
}

#[test]
fn audit_check_gates_on_new_findings() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let baseline = tmp.path().join("base.json");
    let src_arg = src.to_str().unwrap();
    let base_arg = baseline.to_str().unwrap();

    // Start clean: a documented, connected, distinctive symbol (not a finding).
    std::fs::write(
        src.join("lib.rs"),
        r#"
        /// Compute the blake3 fingerprint of a directory tree for staleness.
        pub fn fingerprint_directory_tree() -> u64 { seed() }
        fn seed() -> u64 { 0 }
        "#,
    )
    .unwrap();

    // Snapshot the baseline, then a check against it passes.
    let (ok, _o, e) = run(
        tmp.path(),
        &["audit", src_arg, "--write-baseline", "--baseline", base_arg],
    );
    assert!(ok, "write-baseline should succeed: {e}");
    assert!(baseline.exists(), "baseline file must be written");

    let (ok2, _o2, e2) = run(
        tmp.path(),
        &["audit", src_arg, "--check", "--baseline", base_arg],
    );
    assert!(ok2, "check against a fresh baseline should pass: {e2}");

    // Introduce nine same-named `parse` methods: at least one is buried for its
    // own name (a collision finding) that the baseline never saw.
    std::fs::write(
        src.join("more.rs"),
        r#"
        pub struct P0; impl P0 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P1; impl P1 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P2; impl P2 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P3; impl P3 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P4; impl P4 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P5; impl P5 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P6; impl P6 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P7; impl P7 { pub fn parse(s: &str) -> u8 { 0 } }
        pub struct P8; impl P8 { pub fn parse(s: &str) -> u8 { 0 } }
        "#,
    )
    .unwrap();
    let (ok3, _o3, e3) = run(
        tmp.path(),
        &["audit", src_arg, "--check", "--baseline", base_arg],
    );
    assert!(!ok3, "a new finding must fail --check: stderr={e3}");
    assert!(
        e3.contains("regression"),
        "the gate should report a regression: {e3}"
    );
}
