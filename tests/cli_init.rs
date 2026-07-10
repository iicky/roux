//! End-to-end coverage for `roux init`'s store-scope default: a bare
//! `roux init` must write to the project-local `.roux/db.sqlite` and must
//! never touch the shared global store, while `roux init --global` must do
//! the opposite. Both processes are spawned with `HOME`, `XDG_DATA_HOME`,
//! and `XDG_CONFIG_HOME` pointed at a throwaway temp dir so the global store
//! (`dirs::data_dir()/roux/db.sqlite`) can never resolve into the real
//! developer environment.

use std::path::Path;
use std::process::Command;

use tempfile::tempdir;

/// Run the `roux` binary with `args` in `cwd`, with the global-store
/// location isolated under `home_root`. Returns `true` on a zero exit code.
fn run(cwd: &Path, home_root: &Path, args: &[&str]) -> bool {
    let output = Command::new(env!("CARGO_BIN_EXE_roux"))
        .args(args)
        .current_dir(cwd)
        .env("HOME", home_root)
        .env("XDG_DATA_HOME", home_root.join("data"))
        .env("XDG_CONFIG_HOME", home_root.join("config"))
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn `roux {args:?}`: {e}"));
    output.status.success()
}

/// Recursively search `dir` for a file named `db.sqlite`. Returns `false`
/// if `dir` does not exist.
fn contains_db_sqlite(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if contains_db_sqlite(&path) {
                return true;
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some("db.sqlite") {
            return true;
        }
    }
    false
}

/// Scaffold a manifest-less project: a `src/lib.rs` with no `Cargo.toml`, so
/// `roux init` indexes only the local source and performs no network I/O.
fn write_project(tmp: &Path) {
    let src = tmp.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() -> u32 { 1 }\n").unwrap();
}

#[test]
fn init_defaults_to_local_store() {
    let project = tempdir().unwrap();
    let home_root = tempdir().unwrap();
    write_project(project.path());

    let ok = run(project.path(), home_root.path(), &["init"]);
    assert!(ok, "`roux init` failed");

    // A bare `roux init` must write the project-local store, never the
    // shared global one.
    assert!(
        project.path().join(".roux/db.sqlite").exists(),
        "expected project-local .roux/db.sqlite to be created"
    );
    assert!(
        !contains_db_sqlite(home_root.path()),
        "bare `roux init` must not create any db.sqlite under the global store"
    );
}

#[test]
fn init_global_flag_targets_global_store() {
    let project = tempdir().unwrap();
    let home_root = tempdir().unwrap();
    write_project(project.path());

    let ok = run(project.path(), home_root.path(), &["init", "--global"]);
    assert!(ok, "`roux init --global` failed");

    assert!(
        !project.path().join(".roux/db.sqlite").exists(),
        "`roux init --global` must not create a project-local .roux/db.sqlite"
    );
    assert!(
        contains_db_sqlite(home_root.path()),
        "expected a db.sqlite to be created under the global store"
    );
}
