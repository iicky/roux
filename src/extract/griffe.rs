use std::path::Path;

use anyhow::{Context, Result};

use super::{Extractor, RawChunk};
use crate::source::{Source, SourceKind};

pub struct GriffeExtractor;

impl Extractor for GriffeExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        source.detected_language() == Some("python")
    }

    fn extract(&self, source: &Source) -> Result<Vec<RawChunk>> {
        match &source.kind {
            SourceKind::File(path) => {
                let code = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let module = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                Ok(parse_python(&code, source, &module))
            }
            SourceKind::LocalPath(path) => extract_from_dir(path, source),
            _ => anyhow::bail!("Python extractor only supports local files and directories"),
        }
    }
}

fn extract_from_dir(dir: &Path, source: &Source) -> Result<Vec<RawChunk>> {
    let mut chunks = Vec::new();
    walk_py_files(dir, source, dir, &mut chunks)?;
    Ok(chunks)
}

fn walk_py_files(
    dir: &Path,
    source: &Source,
    base: &Path,
    chunks: &mut Vec<RawChunk>,
) -> Result<()> {
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

        if path.is_dir() {
            // Only recurse into Python packages (dirs with __init__.py)
            if path.join("__init__.py").exists() {
                walk_py_files(&path, source, base, chunks)?;
            }
        } else if path.extension().is_some_and(|e| e == "py") {
            let code = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: skipping unreadable file {}: {e}", path.display());
                    continue;
                }
            };

            let rel = path.strip_prefix(base).unwrap_or(&path);
            let module = rel
                .with_extension("")
                .to_string_lossy()
                .replace(['/', '\\'], ".")
                .replace(".__init__", "");

            chunks.extend(parse_python(&code, source, &module));
        }
    }

    Ok(())
}

/// Parse Python source and extract documented classes, functions, and methods.
/// Uses a simple line-based parser that detects def/class and their docstrings.
fn parse_python(code: &str, source: &Source, module: &str) -> Vec<RawChunk> {
    let source_name = &source.name;
    let source_version = source.version.as_deref().unwrap_or("unknown");
    let prefix = if module.is_empty() || module == "__init__" {
        source_name.clone()
    } else {
        format!("{source_name}.{module}")
    };

    let lines: Vec<&str> = code.lines().collect();
    let mut chunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let stripped = line.trim();

        if let Some(item) = parse_def_or_class(stripped) {
            let indent = indent_level(line);

            // Collect decorators from preceding lines
            let decorators = collect_decorators(&lines, i, indent);

            let docstring = extract_docstring(&lines, i + 1, indent);

            if let Some(doc) = docstring {
                let (item_type, name, mut signature) = match item {
                    PyItem::Function(name, sig) => ("function", name, Some(sig)),
                    PyItem::Class(name) => ("class", name, None),
                };

                // Prepend decorators to signature
                if !decorators.is_empty() {
                    let dec_str = decorators.join("\n");
                    signature = Some(match signature {
                        Some(sig) => format!("{dec_str}\n{sig}"),
                        None => dec_str,
                    });
                }

                let qualified = format!("{prefix}.{name}");
                let body = RawChunk::build_body(item_type, &qualified, signature.as_deref(), &doc);

                chunks.push(RawChunk {
                    source_name: source_name.to_string(),
                    source_version: source_version.to_string(),
                    language: "python".to_string(),
                    item_type: item_type.to_string(),
                    qualified_name: qualified,
                    signature,
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

enum PyItem {
    Function(String, String), // name, full signature
    Class(String),            // name
}

fn parse_def_or_class(line: &str) -> Option<PyItem> {
    if let Some(rest) = line.strip_prefix("def ") {
        let sig = extract_signature(rest)?;
        let name = sig.split('(').next()?.trim().to_string();
        if name.starts_with('_') && !name.starts_with("__") {
            return None;
        }
        Some(PyItem::Function(name, format!("def {sig}")))
    } else if let Some(rest) = line.strip_prefix("class ") {
        let name_end = rest.find([':', '('])?;
        let name = rest[..name_end].trim().to_string();
        if name.starts_with('_') {
            return None;
        }
        Some(PyItem::Class(name))
    } else if let Some(rest) = line.strip_prefix("async def ") {
        let sig = extract_signature(rest)?;
        let name = sig.split('(').next()?.trim().to_string();
        if name.starts_with('_') && !name.starts_with("__") {
            return None;
        }
        Some(PyItem::Function(name, format!("async def {sig}")))
    } else {
        None
    }
}

/// Extract the signature portion before the final `:` that ends the def line.
/// Handles colons inside type annotations like `def f(x: int) -> str:`.
fn extract_signature(rest: &str) -> Option<&str> {
    // Find the last `:` — that's the def terminator
    let sig_end = rest.rfind(':')?;
    Some(rest[..sig_end].trim())
}

/// Collect decorator lines immediately preceding a def/class at the given index.
fn collect_decorators(lines: &[&str], def_index: usize, expected_indent: usize) -> Vec<String> {
    let mut decorators = Vec::new();
    let mut j = def_index;
    while j > 0 {
        j -= 1;
        let line = lines[j];
        let stripped = line.trim();
        if stripped.starts_with('@') && indent_level(line) == expected_indent {
            decorators.push(stripped.to_string());
        } else if stripped.is_empty() {
            continue;
        } else {
            break;
        }
    }
    decorators.reverse();
    decorators
}

fn indent_level(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Extract a docstring that follows a def/class line.
fn extract_docstring(lines: &[&str], start: usize, _parent_indent: usize) -> Option<String> {
    if start >= lines.len() {
        return None;
    }

    let first = lines[start].trim();

    // Single-line docstring: """text""" or '''text'''
    for quote in &["\"\"\"", "'''"] {
        if let Some(rest) = first.strip_prefix(quote) {
            if let Some(content) = rest.strip_suffix(quote) {
                let trimmed = content.trim();
                if trimmed.is_empty() {
                    return None;
                }
                return Some(trimmed.to_string());
            }

            // Multi-line docstring
            let mut doc = rest.to_string();
            for line in &lines[start + 1..] {
                if let Some(before_close) = line.trim().strip_suffix(quote) {
                    if !before_close.is_empty() {
                        doc.push('\n');
                        doc.push_str(before_close.trim());
                    }
                    break;
                }
                doc.push('\n');
                doc.push_str(line.trim());
            }

            let trimmed = doc.trim().to_string();
            if trimmed.is_empty() {
                return None;
            }
            return Some(trimmed);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;

    fn test_source() -> Source {
        Source {
            name: "mylib".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::File(std::path::PathBuf::from("test.py")),
            language: Some("python".to_string()),
        }
    }

    #[test]
    fn test_extract_function() {
        let code = r#"
def greet(name: str) -> str:
    """Say hello to someone."""
    return f"hello {name}"
"#;
        let chunks = parse_python(code, &test_source(), "utils");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "function");
        assert_eq!(chunks[0].qualified_name, "mylib.utils.greet");
        assert_eq!(
            chunks[0].signature.as_deref(),
            Some("def greet(name: str) -> str")
        );
        assert!(chunks[0].doc.contains("Say hello"));
    }

    #[test]
    fn test_extract_class() {
        let code = r#"
class MyModel:
    """A machine learning model."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "models");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "class");
        assert_eq!(chunks[0].qualified_name, "mylib.models.MyModel");
    }

    #[test]
    fn test_skip_private_functions() {
        let code = r#"
def _private():
    """Private."""
    pass

def public():
    """Public."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.mod.public");
    }

    #[test]
    fn test_allow_dunder_methods() {
        let code = r#"
def __init__(self):
    """Initialize."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.mod.__init__");
    }

    #[test]
    fn test_skip_undocumented() {
        let code = r#"
def no_docs():
    pass

def has_docs():
    """Documented."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn test_multiline_docstring() {
        let code = r#"
def complex(x, y):
    """Do something complex.

    Args:
        x: First argument.
        y: Second argument.
    """
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].doc.contains("Do something complex"));
        assert!(chunks[0].doc.contains("Args:"));
    }

    #[test]
    fn test_async_function() {
        let code = r#"
async def fetch(url: str):
    """Fetch a URL."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].signature.as_deref(),
            Some("async def fetch(url: str)")
        );
    }

    #[test]
    fn test_class_with_bases() {
        let code = r#"
class Child(Parent, Mixin):
    """A child class."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.mod.Child");
    }

    #[test]
    fn test_init_module_uses_package_name() {
        let code = r#"
def setup():
    """Set up the package."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "__init__");
        assert_eq!(chunks[0].qualified_name, "mylib.setup");
    }

    #[test]
    fn test_single_quote_docstring() {
        let code = "def thing():\n    '''Single quoted.'''\n    pass\n";
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].doc.contains("Single quoted"));
    }

    #[test]
    fn test_decorator_captured() {
        let code = r#"
@property
def name(self) -> str:
    """Get the name."""
    return self._name
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        let sig = chunks[0].signature.as_deref().unwrap();
        assert!(
            sig.contains("@property"),
            "signature should contain decorator: {sig}"
        );
        assert!(sig.contains("def name(self) -> str"));
    }

    #[test]
    fn test_multiple_decorators() {
        let code = r#"
@staticmethod
@lru_cache(maxsize=128)
def compute(x: int) -> int:
    """Compute something."""
    return x * 2
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        let sig = chunks[0].signature.as_deref().unwrap();
        assert!(sig.contains("@staticmethod"));
        assert!(sig.contains("@lru_cache(maxsize=128)"));
    }

    #[test]
    fn test_class_decorator() {
        let code = r#"
@dataclass
class Config:
    """Configuration."""
    pass
"#;
        let chunks = parse_python(code, &test_source(), "mod");
        assert_eq!(chunks.len(), 1);
        let sig = chunks[0].signature.as_deref().unwrap();
        assert!(sig.contains("@dataclass"));
    }
}
