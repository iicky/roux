use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// A dependency discovered from a lockfile.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    pub version: Option<String>,
    /// Whether this is a direct or transitive dependency.
    pub direct: bool,
}

/// The detected project type and its lockfile path.
#[derive(Debug)]
pub struct ProjectInfo {
    pub kind: ProjectKind,
    pub lockfile: PathBuf,
    pub deps: Vec<Dependency>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProjectKind {
    Rust,
    Node,
    Python,
    Go,
}

impl ProjectKind {
    pub fn language(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Node => "javascript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }
}

/// Detect project type from the current directory by looking for lockfiles/manifests.
pub fn detect_project(dir: &Path) -> Option<ProjectInfo> {
    // Check in priority order — if multiple exist, prefer the primary lockfile
    #[allow(clippy::type_complexity)]
    let checks: &[(&str, ProjectKind, fn(&Path) -> Result<Vec<Dependency>>)] = &[
        ("Cargo.lock", ProjectKind::Rust, parse_cargo_lock),
        ("package-lock.json", ProjectKind::Node, parse_package_lock),
        ("pnpm-lock.yaml", ProjectKind::Node, parse_pnpm_lock),
        ("yarn.lock", ProjectKind::Node, parse_yarn_lock),
        ("poetry.lock", ProjectKind::Python, parse_poetry_lock),
        ("go.sum", ProjectKind::Go, parse_go_sum),
        // Manifests without lockfiles (less precise versions)
        ("pyproject.toml", ProjectKind::Python, parse_pyproject_toml),
        (
            "requirements.txt",
            ProjectKind::Python,
            parse_requirements_txt,
        ),
    ];

    for (filename, kind, parser) in checks {
        let path = dir.join(filename);
        if path.exists() {
            match parser(&path) {
                Ok(deps) => {
                    return Some(ProjectInfo {
                        kind: *kind,
                        lockfile: path,
                        deps,
                    });
                }
                Err(e) => {
                    eprintln!("  warning: failed to parse {filename}: {e}");
                    continue;
                }
            }
        }
    }
    None
}

/// Parse Cargo.lock for Rust dependencies.
fn parse_cargo_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading Cargo.lock")?;
    let mut deps = Vec::new();

    // Also read Cargo.toml to determine direct vs transitive
    let manifest_path = path.parent().unwrap_or(Path::new(".")).join("Cargo.toml");
    let direct_deps = if manifest_path.exists() {
        parse_cargo_toml_deps(&manifest_path).unwrap_or_default()
    } else {
        Vec::new()
    };
    let direct_set: std::collections::HashSet<&str> =
        direct_deps.iter().map(|s| s.as_str()).collect();

    // Parse TOML lockfile
    let lock: toml::Value = content.parse().context("parsing Cargo.lock as TOML")?;
    if let Some(packages) = lock.get("package").and_then(|p| p.as_array()) {
        for pkg in packages {
            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let version = pkg.get("version").and_then(|v| v.as_str());

            if name.is_empty() {
                continue;
            }

            // Skip the root package (source = None means it's the local crate)
            if pkg.get("source").is_none() {
                continue;
            }

            deps.push(Dependency {
                name: name.to_string(),
                version: version.map(|v| v.to_string()),
                direct: direct_set.contains(name),
            });
        }
    }

    Ok(deps)
}

/// Extract direct dependency names from Cargo.toml.
fn parse_cargo_toml_deps(path: &Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(path)?;
    let manifest: toml::Value = content.parse()?;

    let mut deps = Vec::new();
    for section in &["dependencies", "dev-dependencies"] {
        if let Some(table) = manifest.get(section).and_then(|d| d.as_table()) {
            for key in table.keys() {
                deps.push(key.clone());
            }
        }
    }
    Ok(deps)
}

/// Parse package-lock.json for Node.js dependencies.
fn parse_package_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading package-lock.json")?;
    let lock: serde_json::Value = content.parse().context("parsing package-lock.json")?;

    let mut deps = Vec::new();

    // package-lock.json v2/v3 uses "packages" with "" as the root
    if let Some(packages) = lock.get("packages").and_then(|p| p.as_object()) {
        for (key, pkg) in packages {
            // Skip root package
            if key.is_empty() {
                continue;
            }
            // Extract name from path: "node_modules/foo" → "foo"
            let name = key.strip_prefix("node_modules/").unwrap_or(key).to_string();
            // Skip nested node_modules (transitive in node_modules/foo/node_modules/bar)
            let is_transitive = name.contains("node_modules/");
            let version = pkg.get("version").and_then(|v| v.as_str());

            if !is_transitive {
                let direct = pkg
                    .get("dev")
                    .and_then(|d| d.as_bool())
                    .map(|d| !d)
                    .unwrap_or(true);
                deps.push(Dependency {
                    name,
                    version: version.map(|v| v.to_string()),
                    direct,
                });
            }
        }
    }

    Ok(deps)
}

/// Parse pnpm-lock.yaml for Node.js dependencies. Handles the `packages:`
/// section across lockfile versions: keys may be quoted or unquoted (pnpm v9
/// leaves unscoped keys unquoted, e.g. `acorn@8.11.3:`), carry an optional
/// leading `/`, and append a parenthesized peer-deps suffix — all normalized
/// before splitting the name from the version at the last `@`.
fn parse_pnpm_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading pnpm-lock.yaml")?;
    let mut deps = Vec::new();
    let mut in_packages = false;
    let mut key_indent: Option<usize> = None;

    for line in content.lines() {
        if line.trim_end() == "packages:" {
            in_packages = true;
            continue;
        }
        if !in_packages {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            break; // next top-level section (e.g. `snapshots:`) — leave packages
        }
        // Package keys sit at the shallowest indent under `packages:`; deeper
        // lines are that package's fields (resolution/dependencies/…).
        let ki = *key_indent.get_or_insert(indent);
        if indent != ki {
            continue;
        }
        let Some(key) = line.trim().strip_suffix(':') else {
            continue;
        };
        // Normalize: strip quotes, an optional leading '/', and a trailing
        // '(peer@ver)' suffix, then split name from version at the last '@'.
        let key = key.trim_matches('\'').trim_matches('"');
        let key = key.strip_prefix('/').unwrap_or(key);
        let key = key.split('(').next().unwrap_or(key);
        if let Some(at) = key.rfind('@')
            && at > 0
        {
            let name = &key[..at];
            let version = &key[at + 1..];
            if !name.is_empty() {
                deps.push(Dependency {
                    name: name.to_string(),
                    version: Some(version.to_string()),
                    direct: true, // pnpm-lock doesn't easily distinguish
                });
            }
        }
    }

    Ok(deps)
}

/// Parse yarn.lock (Yarn v1 / Berry) for package names. Top-level entries are
/// unindented and end with ':', holding one or more comma-separated
/// `name@range` specifiers; scoped packages (`@scope/name@range`) keep their
/// leading `@scope/` because the name is split from the range at the LAST '@'.
fn parse_yarn_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading yarn.lock")?;
    let mut deps = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in content.lines() {
        if line.starts_with(' ') || line.starts_with('#') || !line.contains('@') {
            continue;
        }
        let Some(key) = line.trim_end().strip_suffix(':') else {
            continue;
        };
        // All specifiers on a key line name the same package; take the first.
        let spec = key
            .split(',')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .trim_matches('\'');
        if let Some(at) = spec.rfind('@')
            && at > 0
        {
            let name = &spec[..at];
            if !name.is_empty() && seen.insert(name.to_string()) {
                deps.push(Dependency {
                    name: name.to_string(),
                    version: None,
                    direct: true, // yarn.lock doesn't distinguish
                });
            }
        }
    }

    Ok(deps)
}

/// Parse poetry.lock for Python dependencies.
fn parse_poetry_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading poetry.lock")?;
    let lock: toml::Value = content.parse().context("parsing poetry.lock as TOML")?;

    let mut deps = Vec::new();
    if let Some(packages) = lock.get("package").and_then(|p| p.as_array()) {
        for pkg in packages {
            let name = pkg.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let version = pkg.get("version").and_then(|v| v.as_str());
            if name.is_empty() {
                continue;
            }
            deps.push(Dependency {
                name: name.to_string(),
                version: version.map(|v| v.to_string()),
                direct: true, // Would need pyproject.toml to determine
            });
        }
    }

    Ok(deps)
}

/// Parse go.sum for Go dependencies.
fn parse_go_sum(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading go.sum")?;
    let mut deps = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            let module = parts[0].to_string();
            let version = parts[1].trim_end_matches("/go.mod").to_string();
            if seen.insert(module.clone()) {
                deps.push(Dependency {
                    name: module,
                    version: Some(version),
                    direct: true, // Would need go.mod to determine
                });
            }
        }
    }

    Ok(deps)
}

/// Parse pyproject.toml for Python dependencies (no lockfile).
fn parse_pyproject_toml(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading pyproject.toml")?;
    let manifest: toml::Value = content.parse().context("parsing pyproject.toml")?;

    let mut deps = Vec::new();

    // PEP 621: [project] dependencies
    if let Some(project_deps) = manifest
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_array())
    {
        for dep in project_deps {
            if let Some(spec) = dep.as_str() {
                // "requests>=2.28" → name="requests"
                let name = spec
                    .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
                    .next()
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    deps.push(Dependency {
                        name,
                        version: None,
                        direct: true,
                    });
                }
            }
        }
    }

    // Poetry: [tool.poetry.dependencies]
    if let Some(poetry_deps) = manifest
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for key in poetry_deps.keys() {
            if key != "python" {
                deps.push(Dependency {
                    name: key.clone(),
                    version: None,
                    direct: true,
                });
            }
        }
    }

    Ok(deps)
}

/// Parse requirements.txt for Python dependencies.
fn parse_requirements_txt(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading requirements.txt")?;
    let mut deps = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('-') {
            continue;
        }
        let name = line
            .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .next()
            .unwrap_or("")
            .to_string();
        if !name.is_empty() {
            deps.push(Dependency {
                name,
                version: None,
                direct: true,
            });
        }
    }

    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cargo_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            r#"
[[package]]
name = "my-project"
version = "0.1.0"

[[package]]
name = "serde"
version = "1.0.200"
source = "registry+https://github.com/rust-lang/crates.io-index"

[[package]]
name = "anyhow"
version = "1.0.80"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#,
        )
        .unwrap();

        let deps = parse_cargo_lock(&dir.path().join("Cargo.lock")).unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].name, "serde");
        assert_eq!(deps[0].version.as_deref(), Some("1.0.200"));
        assert_eq!(deps[1].name, "anyhow");
    }

    #[test]
    fn test_detect_project_rust() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.lock"),
            r#"
[[package]]
name = "my-crate"
version = "0.1.0"

[[package]]
name = "tokio"
version = "1.35.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
"#,
        )
        .unwrap();

        let info = detect_project(dir.path()).unwrap();
        assert_eq!(info.kind, ProjectKind::Rust);
        assert_eq!(info.deps.len(), 1);
        assert_eq!(info.deps[0].name, "tokio");
    }

    #[test]
    fn test_parse_requirements_txt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("requirements.txt"),
            "requests>=2.28\nflask==2.3.0\n# comment\nnumpy\n",
        )
        .unwrap();

        let deps = parse_requirements_txt(&dir.path().join("requirements.txt")).unwrap();
        assert_eq!(deps.len(), 3);
        assert_eq!(deps[0].name, "requests");
        assert_eq!(deps[1].name, "flask");
        assert_eq!(deps[2].name, "numpy");
    }

    #[test]
    fn test_parse_yarn_lock_scoped_packages() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("yarn.lock"),
            r#"
"@babel/core@^7.0.0", "@babel/core@^7.1.0":
  version "7.23.0"

"@types/node@^18.0.0":
  version "18.19.0"

lodash@^4.17.21:
  version "4.17.21"
"#,
        )
        .unwrap();

        let deps = parse_yarn_lock(&dir.path().join("yarn.lock")).unwrap();
        let names: Vec<&str> = deps.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["@babel/core", "@types/node", "lodash"]);
        assert_eq!(
            deps.iter().filter(|d| d.name == "@babel/core").count(),
            1,
            "scoped package listed under two specifiers must be deduplicated"
        );
    }

    #[test]
    fn test_parse_pnpm_lock_v9_unquoted_scoped_and_peer_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-lock.yaml"),
            r#"lockfileVersion: '9.0'

importers:

  .:
    dependencies:
      acorn:
        specifier: ^8.11.3
        version: 8.11.3

packages:

  acorn@8.11.3:
    resolution: {integrity: sha512-fake-acorn==}
    engines: {node: '>=0.4.0'}

  '@babel/core@7.23.0':
    resolution: {integrity: sha512-fake-babel-core==}

  react-dom@18.2.0(react@18.2.0):
    resolution: {integrity: sha512-fake-react-dom==}

snapshots:

  acorn@8.11.3: {}

  some-snapshot-only@2.0.0:
    dependencies:
      acorn: 8.11.3
"#,
        )
        .unwrap();

        let deps = parse_pnpm_lock(&dir.path().join("pnpm-lock.yaml")).unwrap();
        assert_eq!(deps.len(), 3);

        let acorn = deps.iter().find(|d| d.name == "acorn").unwrap();
        assert_eq!(acorn.version.as_deref(), Some("8.11.3"));

        let babel = deps.iter().find(|d| d.name == "@babel/core").unwrap();
        assert_eq!(babel.version.as_deref(), Some("7.23.0"));

        let react_dom = deps.iter().find(|d| d.name == "react-dom").unwrap();
        assert_eq!(
            react_dom.version.as_deref(),
            Some("18.2.0"),
            "peer suffix must be stripped from the version"
        );

        assert!(
            !deps.iter().any(|d| d.name == "some-snapshot-only"),
            "packages that only appear under `snapshots:` must not be parsed"
        );
    }

    #[test]
    fn test_parse_pnpm_lock_leading_slash_scoped_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pnpm-lock.yaml"),
            r#"lockfileVersion: '9.0'

packages:

  /@scope/pkg@1.2.3:
    resolution: {integrity: sha512-fake-scope-pkg==}
"#,
        )
        .unwrap();

        let deps = parse_pnpm_lock(&dir.path().join("pnpm-lock.yaml")).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "@scope/pkg");
        assert_eq!(deps[0].version.as_deref(), Some("1.2.3"));
    }
}
