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

/// Parse yarn.lock — simplified extraction.
/// Parse pnpm-lock.yaml for Node.js dependencies.
fn parse_pnpm_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading pnpm-lock.yaml")?;
    let mut deps = Vec::new();
    let mut in_packages = false;

    for line in content.lines() {
        if line == "packages:" {
            in_packages = true;
            continue;
        }
        if in_packages && !line.starts_with(' ') && !line.is_empty() {
            break; // left the packages section
        }
        if !in_packages {
            continue;
        }

        // Match lines like: '  @scope/name@1.2.3':  or  'name@1.2.3':
        let trimmed = line.trim();
        if let Some(entry) = trimmed
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix("':"))
        {
            // Split on last '@' to separate name from version
            if let Some(at_pos) = entry.rfind('@') {
                let name = &entry[..at_pos];
                let version = &entry[at_pos + 1..];
                if !name.is_empty() {
                    deps.push(Dependency {
                        name: name.to_string(),
                        version: Some(version.to_string()),
                        direct: true, // pnpm-lock doesn't easily distinguish
                    });
                }
            }
        }
    }

    Ok(deps)
}

fn parse_yarn_lock(path: &Path) -> Result<Vec<Dependency>> {
    let content = std::fs::read_to_string(path).context("reading yarn.lock")?;
    let mut deps = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in content.lines() {
        // yarn.lock entries look like: "package-name@^1.0.0":
        if !line.starts_with(' ') && !line.starts_with('#') && line.contains('@') {
            let name = line
                .trim_matches('"')
                .split('@')
                .next()
                .unwrap_or("")
                .to_string();
            if !name.is_empty() && seen.insert(name.clone()) {
                deps.push(Dependency {
                    name,
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
}
