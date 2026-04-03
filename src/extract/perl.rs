use std::path::Path;

use anyhow::{Context, Result};

use super::{Extractor, RawChunk};
use crate::source::{Source, SourceKind};

pub struct PerlExtractor;

impl Extractor for PerlExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.detected_language() == Some("perl")
    }

    fn extract(&self, source: &Source) -> Result<Vec<RawChunk>> {
        match &source.kind {
            SourceKind::File(path) => {
                let code = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let module = module_from_path(path);
                Ok(parse_perl(&code, source, &module))
            }
            SourceKind::LocalPath(path) => extract_from_dir(path, source),
            _ => anyhow::bail!("Perl extractor only supports local files and directories"),
        }
    }
}

/// Build a module path from a file path: replace `/` with `::`, strip `.pm`/`.pl`.
fn module_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn extract_from_dir(dir: &Path, source: &Source) -> Result<Vec<RawChunk>> {
    let mut chunks = Vec::new();
    walk_perl_files(dir, source, dir, &mut chunks, 0)?;
    Ok(chunks)
}

fn walk_perl_files(
    dir: &Path,
    source: &Source,
    base: &Path,
    chunks: &mut Vec<RawChunk>,
    depth: usize,
) -> Result<()> {
    if depth > super::MAX_WALK_DEPTH {
        eprintln!("warning: max directory depth reached at {}", dir.display());
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!(
                "warning: skipping unreadable directory {}: {e}",
                dir.display()
            );
            return Ok(());
        }
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();

        if super::is_symlink(&path) {
            eprintln!("warning: skipping symlink {}", path.display());
            continue;
        }

        if path.is_dir() {
            walk_perl_files(&path, source, base, chunks, depth + 1)?;
        } else if path.extension().is_some_and(|e| e == "pl" || e == "pm") {
            let code = match super::safe_read_file(&path) {
                Some(c) => c,
                None => continue,
            };

            let rel = path.strip_prefix(base).unwrap_or(&path);
            let module = rel
                .with_extension("")
                .to_string_lossy()
                .replace(['/', '\\'], "::");

            chunks.extend(parse_perl(&code, source, &module));
        }
    }

    Ok(())
}

/// Parsed Perl item types.
enum PerlItem {
    Sub(String, String), // name, signature line
    Package(String),     // package name
}

/// Parse Perl source and extract documented subs, packages, and POD sections.
fn parse_perl(code: &str, source: &Source, module: &str) -> Vec<RawChunk> {
    let source_name = &source.name;
    let source_version = source.version.as_deref().unwrap_or("unknown");
    let prefix = if module.is_empty() {
        source_name.clone()
    } else {
        format!("{source_name}::{module}")
    };

    let lines: Vec<&str> = code.lines().collect();
    let mut chunks = Vec::new();
    let mut current_package: Option<String> = None;
    let mut i = 0;

    // First pass: extract POD documentation blocks
    chunks.extend(extract_pod_sections(
        &lines,
        source_name,
        source_version,
        &prefix,
    ));

    // Second pass: extract subs and packages
    while i < lines.len() {
        let stripped = lines[i].trim();

        // Track current package
        if let Some(pkg) = parse_package(stripped) {
            current_package = Some(pkg.clone());

            // Look for POD or comment doc immediately preceding the package line
            let doc = collect_preceding_comments(&lines, i);
            if let Some(doc) = doc {
                let qualified = format!("{prefix}::{pkg}");
                let body = RawChunk::build_body("package", &qualified, None, &doc);
                chunks.push(RawChunk {
                    source_name: source_name.to_string(),
                    source_version: source_version.to_string(),
                    language: "perl".to_string(),
                    item_type: "package".to_string(),
                    qualified_name: qualified,
                    signature: Some(format!("package {pkg};")),
                    doc,
                    body,
                    url: None,
                });
            }
            i += 1;
            continue;
        }

        // Parse sub declarations
        if let Some(PerlItem::Sub(name, sig)) = parse_sub(stripped) {
            let doc = collect_preceding_comments(&lines, i);
            if let Some(doc) = doc {
                let qualified = if let Some(ref pkg) = current_package {
                    format!("{prefix}::{pkg}::{name}")
                } else {
                    format!("{prefix}::{name}")
                };

                let body = RawChunk::build_body("function", &qualified, Some(&sig), &doc);
                chunks.push(RawChunk {
                    source_name: source_name.to_string(),
                    source_version: source_version.to_string(),
                    language: "perl".to_string(),
                    item_type: "function".to_string(),
                    qualified_name: qualified,
                    signature: Some(sig),
                    doc,
                    body,
                    url: None,
                });
            }
        }

        i += 1;
    }

    chunks
}

/// Parse a `package Foo::Bar;` declaration.
fn parse_package(line: &str) -> Option<String> {
    let rest = line.strip_prefix("package ")?;
    let name = rest.trim_end_matches(';').trim();
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Parse a `sub foo { ... }` declaration.
fn parse_sub(line: &str) -> Option<PerlItem> {
    let rest = line.strip_prefix("sub ")?;
    // sub name, possibly followed by { or (prototype)
    let name_end = rest.find(|c: char| c == '{' || c == '(' || c == ';' || c.is_whitespace())?;
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    // Build signature from the whole line
    let sig = format!("sub {}", rest.split('{').next().unwrap_or(&name).trim());
    Some(PerlItem::Sub(name, sig))
}

/// Collect comment block (`#` lines) immediately preceding the given line index.
fn collect_preceding_comments(lines: &[&str], index: usize) -> Option<String> {
    let mut comment_lines = Vec::new();
    let mut j = index;
    while j > 0 {
        j -= 1;
        let stripped = lines[j].trim();
        if let Some(comment) = stripped.strip_prefix('#') {
            comment_lines.push(comment.trim().to_string());
        } else if stripped.is_empty() {
            continue;
        } else {
            break;
        }
    }

    if comment_lines.is_empty() {
        return None;
    }
    comment_lines.reverse();
    Some(comment_lines.join("\n"))
}

/// Extract POD documentation sections (=head1, =head2, content until =cut).
fn extract_pod_sections(
    lines: &[&str],
    source_name: &str,
    source_version: &str,
    prefix: &str,
) -> Vec<RawChunk> {
    let mut chunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let stripped = lines[i].trim();

        if let Some(heading) = stripped
            .strip_prefix("=head1 ")
            .or_else(|| stripped.strip_prefix("=head2 "))
        {
            let heading_name = heading.trim().to_string();
            let heading_level = if stripped.starts_with("=head1") {
                "head1"
            } else {
                "head2"
            };

            // Collect content until =cut or next =head
            let mut content = Vec::new();
            i += 1;
            while i < lines.len() {
                let line = lines[i].trim();
                if line == "=cut" || line.starts_with("=head1 ") || line.starts_with("=head2 ") {
                    break;
                }
                content.push(lines[i]);
                i += 1;
            }

            let doc = content
                .iter()
                .map(|l| l.trim())
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();

            if !doc.is_empty() {
                let qualified = format!("{prefix}::{heading_name}");
                let body = RawChunk::build_body(heading_level, &qualified, None, &doc);
                chunks.push(RawChunk {
                    source_name: source_name.to_string(),
                    source_version: source_version.to_string(),
                    language: "perl".to_string(),
                    item_type: heading_level.to_string(),
                    qualified_name: qualified,
                    signature: None,
                    doc,
                    body,
                    url: None,
                });
            }
            // Don't increment i here — the while loop will re-check the current line
            // which might be another =head
            continue;
        }

        i += 1;
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;

    fn test_source() -> Source {
        Source {
            name: "mylib".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::File(std::path::PathBuf::from("test.pl")),
            language: Some("perl".to_string()),
        }
    }

    #[test]
    fn test_extract_sub_with_comment() {
        let code = r#"
# Greets a person by name.
sub greet {
    my ($name) = @_;
    print "Hello, $name\n";
}
"#;
        let chunks = parse_perl(code, &test_source(), "utils");
        let subs: Vec<_> = chunks
            .iter()
            .filter(|c| c.item_type == "function")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].qualified_name, "mylib::utils::greet");
        assert_eq!(subs[0].signature.as_deref(), Some("sub greet"));
        assert!(subs[0].doc.contains("Greets a person"));
    }

    #[test]
    fn test_extract_package() {
        let code = r#"
# The Foo::Bar package provides bar utilities.
package Foo::Bar;
"#;
        let chunks = parse_perl(code, &test_source(), "lib");
        let pkgs: Vec<_> = chunks.iter().filter(|c| c.item_type == "package").collect();
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].qualified_name, "mylib::lib::Foo::Bar");
        assert!(pkgs[0].doc.contains("bar utilities"));
    }

    #[test]
    fn test_sub_inside_package() {
        let code = r#"
package MyPkg;

# Does something useful.
sub do_thing {
    return 1;
}
"#;
        let chunks = parse_perl(code, &test_source(), "lib");
        let subs: Vec<_> = chunks
            .iter()
            .filter(|c| c.item_type == "function")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].qualified_name, "mylib::lib::MyPkg::do_thing");
    }

    #[test]
    fn test_skip_undocumented_sub() {
        let code = r#"
sub no_docs {
    return 1;
}

# Has docs.
sub with_docs {
    return 2;
}
"#;
        let chunks = parse_perl(code, &test_source(), "mod");
        let subs: Vec<_> = chunks
            .iter()
            .filter(|c| c.item_type == "function")
            .collect();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].qualified_name, "mylib::mod::with_docs");
    }

    #[test]
    fn test_pod_head1() {
        let code = r#"
=head1 NAME

MyModule - A great module

=cut
"#;
        let chunks = parse_perl(code, &test_source(), "MyModule");
        let pods: Vec<_> = chunks.iter().filter(|c| c.item_type == "head1").collect();
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].qualified_name, "mylib::MyModule::NAME");
        assert!(pods[0].doc.contains("A great module"));
    }

    #[test]
    fn test_pod_head2() {
        let code = r#"
=head2 new

Creates a new instance.

=cut
"#;
        let chunks = parse_perl(code, &test_source(), "Foo");
        let pods: Vec<_> = chunks.iter().filter(|c| c.item_type == "head2").collect();
        assert_eq!(pods.len(), 1);
        assert_eq!(pods[0].qualified_name, "mylib::Foo::new");
        assert!(pods[0].doc.contains("Creates a new instance"));
    }

    #[test]
    fn test_multiple_pod_sections() {
        let code = r#"
=head1 NAME

Foo - A foo module

=head1 DESCRIPTION

This module does foo things.

=cut
"#;
        let chunks = parse_perl(code, &test_source(), "Foo");
        let pods: Vec<_> = chunks.iter().filter(|c| c.item_type == "head1").collect();
        assert_eq!(pods.len(), 2);
    }

    #[test]
    fn test_empty_file() {
        let chunks = parse_perl("", &test_source(), "mod");
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_multiline_comment_block() {
        let code = r#"
# This function does multiple things:
# 1. First thing
# 2. Second thing
sub multi {
    return 1;
}
"#;
        let chunks = parse_perl(code, &test_source(), "mod");
        let subs: Vec<_> = chunks
            .iter()
            .filter(|c| c.item_type == "function")
            .collect();
        assert_eq!(subs.len(), 1);
        assert!(subs[0].doc.contains("1. First thing"));
        assert!(subs[0].doc.contains("2. Second thing"));
    }

    #[test]
    fn test_can_handle_perl_source() {
        let ext = PerlExtractor;
        let perl_source = Source {
            name: "lib".to_string(),
            version: None,
            kind: SourceKind::File(std::path::PathBuf::from("main.pl")),
            language: Some("perl".to_string()),
        };
        assert!(ext.can_handle(&perl_source));

        let py_source = Source {
            name: "lib".to_string(),
            version: None,
            kind: SourceKind::File(std::path::PathBuf::from("main.py")),
            language: Some("python".to_string()),
        };
        assert!(!ext.can_handle(&py_source));
    }

    #[test]
    fn test_extract_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        let subdir = dir.path().join("Lib");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(
            subdir.join("Util.pm"),
            "# Adds two numbers.\nsub add {\n    return $_[0] + $_[1];\n}\n",
        )
        .unwrap();

        let source = Source {
            name: "mylib".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::LocalPath(dir.path().to_path_buf()),
            language: Some("perl".to_string()),
        };
        let ext = PerlExtractor;
        let chunks = ext.extract(&source).unwrap();
        let subs: Vec<_> = chunks
            .iter()
            .filter(|c| c.item_type == "function")
            .collect();
        assert_eq!(subs.len(), 1);
        assert!(subs[0].qualified_name.contains("Lib::Util::add"));
    }

    #[test]
    fn test_module_path_from_file() {
        let path = Path::new("Foo/Bar.pm");
        assert_eq!(module_from_path(path), "Bar");
    }
}
