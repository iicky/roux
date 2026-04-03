use std::path::Path;

use anyhow::{Context, Result};

use super::{Extractor, RawChunk};
use crate::source::{Source, SourceKind};

pub struct MarkdownExtractor;

impl Extractor for MarkdownExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.format_hint() == Some("markdown")
    }

    fn extract(&self, source: &Source) -> Result<Vec<RawChunk>> {
        match &source.kind {
            SourceKind::File(path) => {
                let content = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                Ok(parse_markdown(&content, source))
            }
            SourceKind::LocalPath(path) => extract_from_dir(path, source),
            _ => anyhow::bail!("MarkdownExtractor only supports local files and directories"),
        }
    }
}

fn extract_from_dir(dir: &Path, source: &Source) -> Result<Vec<RawChunk>> {
    let mut chunks = Vec::new();
    walk_md_files(dir, source, &mut chunks, 0)?;
    Ok(chunks)
}

fn walk_md_files(
    dir: &Path,
    source: &Source,
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
            walk_md_files(&path, source, chunks, depth + 1)?;
        } else if path
            .extension()
            .is_some_and(|e| e == "md" || e == "markdown")
        {
            let content = match super::safe_read_file(&path) {
                Some(c) => c,
                None => continue,
            };
            chunks.extend(parse_markdown(&content, source));
        }
    }

    Ok(())
}

/// Parse markdown content into chunks, splitting on headings.
fn parse_markdown(content: &str, source: &Source) -> Vec<RawChunk> {
    let source_name = &source.name;
    let source_version = source.version.as_deref().unwrap_or("unknown");
    let mut chunks = Vec::new();
    let mut heading_stack: Vec<(usize, String)> = Vec::new();
    let mut current_body = String::new();

    for line in content.lines() {
        if let Some(heading) = parse_heading(line) {
            // Flush previous section
            flush_section(
                &heading_stack,
                &current_body,
                source_name,
                source_version,
                &mut chunks,
            );
            current_body.clear();

            // Update heading stack
            let level = heading.0;
            while heading_stack.last().is_some_and(|(l, _)| *l >= level) {
                heading_stack.pop();
            }
            heading_stack.push(heading);
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // Flush final section
    flush_section(
        &heading_stack,
        &current_body,
        source_name,
        source_version,
        &mut chunks,
    );

    chunks
}

fn flush_section(
    heading_stack: &[(usize, String)],
    body: &str,
    source_name: &str,
    source_version: &str,
    chunks: &mut Vec<RawChunk>,
) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }

    let (full_qualified, heading_text) = if heading_stack.is_empty() {
        // Content before first heading gets an implicit preamble section
        (
            format!("{source_name} > (preamble)"),
            "(preamble)".to_string(),
        )
    } else {
        let qualified_name: String = heading_stack
            .iter()
            .map(|(_, h)| h.as_str())
            .collect::<Vec<_>>()
            .join(" > ");
        let heading = heading_stack
            .last()
            .map(|(_, h)| h.clone())
            .unwrap_or_default();
        (format!("{source_name} > {qualified_name}"), heading)
    };
    let body_text = RawChunk::build_body("section", &full_qualified, Some(&heading_text), trimmed);

    chunks.push(RawChunk {
        source_name: source_name.to_string(),
        source_version: source_version.to_string(),
        language: "markdown".to_string(),
        item_type: "section".to_string(),
        qualified_name: full_qualified,
        signature: Some(heading_text),
        doc: trimmed.to_string(),
        body: body_text,
        url: None,
    });
}

/// Parse a markdown heading line, returning (level, text).
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('#') {
        return None;
    }
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if level > 6 {
        return None;
    }
    let text = trimmed[level..].trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some((level, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;

    fn test_source() -> Source {
        Source {
            name: "docs".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::File(std::path::PathBuf::from("test.md")),
            language: Some("markdown".to_string()),
        }
    }

    #[test]
    fn test_parse_heading() {
        assert_eq!(parse_heading("# Hello"), Some((1, "Hello".to_string())));
        assert_eq!(
            parse_heading("## Sub heading"),
            Some((2, "Sub heading".to_string()))
        );
        assert_eq!(parse_heading("not a heading"), None);
        assert_eq!(parse_heading("#"), None); // empty heading
    }

    #[test]
    fn test_simple_sections() {
        let md = "# Getting Started\nInstall the thing.\n\n# Usage\nRun the thing.\n";
        let chunks = parse_markdown(md, &test_source());
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].qualified_name, "docs > Getting Started");
        assert!(chunks[0].doc.contains("Install the thing"));
        assert_eq!(chunks[1].qualified_name, "docs > Usage");
    }

    #[test]
    fn test_nested_headings() {
        let md = "# Guide\nIntro.\n## Install\nDo this.\n## Config\nDo that.\n";
        let chunks = parse_markdown(md, &test_source());
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].qualified_name, "docs > Guide");
        assert_eq!(chunks[1].qualified_name, "docs > Guide > Install");
        assert_eq!(chunks[2].qualified_name, "docs > Guide > Config");
    }

    #[test]
    fn test_heading_stack_resets_on_same_level() {
        let md = "# A\n## B\ntext\n## C\ntext\n";
        let chunks = parse_markdown(md, &test_source());
        // A (empty body, skipped), B, C
        let names: Vec<&str> = chunks.iter().map(|c| c.qualified_name.as_str()).collect();
        assert!(names.contains(&"docs > A > B"));
        assert!(names.contains(&"docs > A > C"));
        // C should NOT be "A > B > C"
        assert!(!names.contains(&"docs > A > B > C"));
    }

    #[test]
    fn test_skips_empty_sections() {
        let md = "# Empty\n# Has Content\nSomething here.\n";
        let chunks = parse_markdown(md, &test_source());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "docs > Has Content");
    }

    #[test]
    fn test_content_before_first_heading_captured() {
        let md = "Some preamble.\n# Real Section\nContent.\n";
        let chunks = parse_markdown(md, &test_source());
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].qualified_name, "docs > (preamble)");
        assert!(chunks[0].doc.contains("Some preamble"));
        assert_eq!(chunks[1].qualified_name, "docs > Real Section");
    }

    #[test]
    fn test_preamble_only_document() {
        let md = "Just some text with no headings at all.\n";
        let chunks = parse_markdown(md, &test_source());
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "docs > (preamble)");
    }

    #[test]
    fn test_empty_document() {
        let chunks = parse_markdown("", &test_source());
        assert!(chunks.is_empty());

        let chunks = parse_markdown("   \n\n  \n", &test_source());
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_deeply_nested_headings() {
        let md = "# A\n## B\n### C\nDeep content.\n";
        let chunks = parse_markdown(md, &test_source());
        let deep = chunks
            .iter()
            .find(|c| c.doc.contains("Deep content"))
            .unwrap();
        assert_eq!(deep.qualified_name, "docs > A > B > C");
    }

    #[test]
    fn test_heading_level_jump_back() {
        // h3 followed by h1 should reset the stack
        let md = "# A\n## B\n### C\nContent C.\n# D\nContent D.\n";
        let chunks = parse_markdown(md, &test_source());
        let d = chunks.iter().find(|c| c.doc.contains("Content D")).unwrap();
        assert_eq!(d.qualified_name, "docs > D");
    }

    #[test]
    fn test_can_handle_markdown_source() {
        let ext = MarkdownExtractor;
        let md_source = Source {
            name: "docs".to_string(),
            version: None,
            kind: SourceKind::File(std::path::PathBuf::from("README.md")),
            language: Some("markdown".to_string()),
        };
        assert!(ext.can_handle(&md_source));

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
        std::fs::write(dir.path().join("one.md"), "# First\nContent one.\n").unwrap();
        std::fs::write(dir.path().join("two.md"), "# Second\nContent two.\n").unwrap();
        std::fs::write(dir.path().join("ignore.txt"), "not markdown").unwrap();

        let source = Source {
            name: "docs".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::LocalPath(dir.path().to_path_buf()),
            language: Some("markdown".to_string()),
        };
        let ext = MarkdownExtractor;
        let chunks = ext.extract(&source).unwrap();
        assert_eq!(chunks.len(), 2);
    }
}
