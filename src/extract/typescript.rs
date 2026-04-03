use std::path::Path;

use anyhow::{Context, Result};

use super::{Extractor, RawChunk};
use crate::source::{Source, SourceKind};

const TS_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs"];

pub struct TypeScriptExtractor;

impl Extractor for TypeScriptExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        matches!(
            source.detected_language(),
            Some("typescript" | "javascript")
        )
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
                let lang = lang_from_path(path);
                Ok(parse_ts(&code, source, &module, lang))
            }
            SourceKind::LocalPath(path) => extract_from_dir(path, source),
            _ => anyhow::bail!(
                "TypeScript/JavaScript extractor only supports local files and directories"
            ),
        }
    }
}

fn lang_from_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("ts" | "tsx") => "typescript",
        _ => "javascript",
    }
}

fn is_ts_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| TS_EXTENSIONS.contains(&ext))
}

fn extract_from_dir(dir: &Path, source: &Source) -> Result<Vec<RawChunk>> {
    let mut chunks = Vec::new();
    walk_ts_files(dir, source, dir, &mut chunks, 0)?;
    Ok(chunks)
}

fn walk_ts_files(
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
            // Skip node_modules and hidden directories
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "node_modules" || name.starts_with('.') {
                continue;
            }
            walk_ts_files(&path, source, base, chunks, depth + 1)?;
        } else if is_ts_file(&path) {
            let code = match super::safe_read_file(&path) {
                Some(c) => c,
                None => continue,
            };

            let rel = path.strip_prefix(base).unwrap_or(&path);
            let module = rel
                .with_extension("")
                .to_string_lossy()
                .replace(['/', '\\'], ".")
                .trim_end_matches(".index")
                .to_string();

            let lang = lang_from_path(&path);
            chunks.extend(parse_ts(&code, source, &module, lang));
        }
    }

    Ok(())
}

/// Parse TypeScript/JavaScript source and extract documented exported items.
fn parse_ts(code: &str, source: &Source, module: &str, lang: &str) -> Vec<RawChunk> {
    let source_name = &source.name;
    let source_version = source.version.as_deref().unwrap_or("unknown");
    let prefix = if module.is_empty() || module == "index" {
        source_name.clone()
    } else {
        format!("{source_name}.{module}")
    };

    let lines: Vec<&str> = code.lines().collect();
    let mut chunks = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let stripped = lines[i].trim();

        if let Some(item) = parse_declaration(stripped) {
            // Look for JSDoc comment preceding this declaration
            let jsdoc = collect_jsdoc(&lines, i);

            if let Some(doc) = jsdoc {
                let (item_type, name, signature) = match item {
                    TsItem::Function(name, sig) => ("function", name, Some(sig)),
                    TsItem::Class(name, sig) => ("class", name, Some(sig)),
                    TsItem::Const(name, sig) => ("function", name, Some(sig)),
                };

                let qualified = format!("{prefix}.{name}");
                let body = RawChunk::build_body(item_type, &qualified, signature.as_deref(), &doc);

                chunks.push(RawChunk {
                    source_name: source_name.to_string(),
                    source_version: source_version.to_string(),
                    language: lang.to_string(),
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

enum TsItem {
    Function(String, String), // name, signature
    Class(String, String),    // name, signature
    Const(String, String),    // name, signature (for arrow fns)
}

/// Parse an exported declaration line.
fn parse_declaration(line: &str) -> Option<TsItem> {
    // Strip leading "export default " or "export "
    let (is_export, rest) = if let Some(r) = line.strip_prefix("export default ") {
        (true, r)
    } else if let Some(r) = line.strip_prefix("export ") {
        (true, r)
    } else {
        (false, line)
    };

    // Only extract exported items
    if !is_export {
        return None;
    }

    // "async function name(...)" or "function name(...)"
    if let Some(r) = rest.strip_prefix("async function") {
        let r = r.trim_start();
        // might be "async function*" for generators
        let r = r.strip_prefix('*').map(|s| s.trim_start()).unwrap_or(r);
        return parse_function_sig(r, "async function");
    }
    if let Some(r) = rest.strip_prefix("function") {
        let r = r.trim_start();
        let r = r.strip_prefix('*').map(|s| s.trim_start()).unwrap_or(r);
        return parse_function_sig(r, "function");
    }

    // "class Name extends/implements ..."
    if let Some(r) = rest.strip_prefix("class ") {
        let r = r.trim();
        // Name ends at whitespace, {, <, or end of line
        let name_end = r
            .find(|c: char| c.is_whitespace() || c == '{' || c == '<')
            .unwrap_or(r.len());
        let name = r[..name_end].to_string();
        if name.is_empty() {
            return None;
        }
        // Signature up to '{'
        let sig_end = r.find('{').unwrap_or(r.len());
        let sig = format!("class {}", r[..sig_end].trim());
        return Some(TsItem::Class(name, sig));
    }

    // "const name = ..." (arrow function or value)
    if let Some(r) = rest.strip_prefix("const ") {
        return parse_const_declaration(r);
    }

    None
}

/// Parse "name(...): ReturnType {" from a function declaration.
fn parse_function_sig(rest: &str, keyword: &str) -> Option<TsItem> {
    // Name ends at '(' or '<' (generics)
    let name_end = rest.find(['(', '<'])?;
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    // Signature is everything up to '{'
    let sig_end = rest.find('{').unwrap_or(rest.len());
    let sig_part = rest[..sig_end].trim();
    let sig = format!("{keyword} {sig_part}");
    Some(TsItem::Function(name, sig))
}

/// Parse "name: Type = (...) => ..." or "name = (...) => ..."
fn parse_const_declaration(rest: &str) -> Option<TsItem> {
    // Get the name (up to ':', '=', or whitespace after name)
    let rest = rest.trim();
    let name_end = rest.find([':', '=', ' ']).unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }

    // Check if this looks like an arrow function (contains "=>")
    if rest.contains("=>") {
        let sig_end = rest
            .find('{')
            .or_else(|| rest.find("=>"))
            .unwrap_or(rest.len());
        // Include the => for arrow functions
        let sig = if rest.find("=>").is_some_and(|pos| pos >= sig_end) {
            let arrow_pos = rest.find("=>").unwrap();
            format!("const {}", rest[..arrow_pos + 2].trim())
        } else {
            format!("const {}", rest[..sig_end].trim())
        };
        return Some(TsItem::Const(name, sig));
    }

    None
}

/// Collect a JSDoc comment (`/** ... */`) immediately before the given line index.
fn collect_jsdoc(lines: &[&str], decl_index: usize) -> Option<String> {
    if decl_index == 0 {
        return None;
    }

    // Walk backwards to find the end of a JSDoc block
    let mut end = decl_index;
    while end > 0 {
        end -= 1;
        let trimmed = lines[end].trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.ends_with("*/") {
            break;
        }
        // Not a JSDoc comment
        return None;
    }

    let end_line = end;
    let end_trimmed = lines[end_line].trim();

    // Single-line JSDoc: /** text */
    if end_trimmed.starts_with("/**") && end_trimmed.ends_with("*/") {
        let content = end_trimmed
            .strip_prefix("/**")
            .unwrap()
            .strip_suffix("*/")
            .unwrap()
            .trim();
        if content.is_empty() {
            return None;
        }
        return Some(content.to_string());
    }

    // Multi-line JSDoc: find the opening /**
    let mut start = end_line;
    while start > 0 {
        start -= 1;
        let trimmed = lines[start].trim();
        if trimmed.starts_with("/**") {
            break;
        }
        if !trimmed.starts_with('*') && !trimmed.starts_with("* ") {
            // If we hit a non-JSDoc line, give up
            if !trimmed.starts_with('*') {
                return None;
            }
        }
    }

    // Extract text from the JSDoc block
    let mut doc_lines = Vec::new();
    for line in &lines[start..=end_line] {
        let trimmed = line.trim();
        let content = trimmed
            .strip_prefix("/**")
            .or_else(|| trimmed.strip_prefix("*/"))
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix('*'))
            .unwrap_or(trimmed);
        let content = content.strip_suffix("*/").unwrap_or(content).trim();
        if !content.is_empty() {
            doc_lines.push(content.to_string());
        }
    }

    if doc_lines.is_empty() {
        return None;
    }

    Some(doc_lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceKind;

    fn test_source() -> Source {
        Source {
            name: "mylib".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::File(std::path::PathBuf::from("test.ts")),
            language: Some("typescript".to_string()),
        }
    }

    #[test]
    fn test_export_function() {
        let code = r#"
/** Greet someone by name. */
export function greet(name: string): string {
    return `hello ${name}`;
}
"#;
        let chunks = parse_ts(code, &test_source(), "utils", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "function");
        assert_eq!(chunks[0].qualified_name, "mylib.utils.greet");
        assert!(
            chunks[0]
                .signature
                .as_deref()
                .unwrap()
                .contains("function greet(name: string): string")
        );
        assert!(chunks[0].doc.contains("Greet someone"));
    }

    #[test]
    fn test_export_class() {
        let code = r#"
/** A user model. */
export class User extends BaseModel {
    name: string;
}
"#;
        let chunks = parse_ts(code, &test_source(), "models", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "class");
        assert_eq!(chunks[0].qualified_name, "mylib.models.User");
        assert!(
            chunks[0]
                .signature
                .as_deref()
                .unwrap()
                .contains("class User extends BaseModel")
        );
    }

    #[test]
    fn test_export_default_function() {
        let code = r#"
/** The main handler. */
export default function handler(req: Request): Response {
    return new Response("ok");
}
"#;
        let chunks = parse_ts(code, &test_source(), "api", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.api.handler");
    }

    #[test]
    fn test_export_async_function() {
        let code = r#"
/** Fetch data from the API. */
export async function fetchData(url: string): Promise<Data> {
    return fetch(url).then(r => r.json());
}
"#;
        let chunks = parse_ts(code, &test_source(), "mod", "typescript");
        assert_eq!(chunks.len(), 1);
        assert!(
            chunks[0]
                .signature
                .as_deref()
                .unwrap()
                .contains("async function")
        );
    }

    #[test]
    fn test_export_const_arrow() {
        let code = r#"
/** Add two numbers. */
export const add = (a: number, b: number): number => {
    return a + b;
};
"#;
        let chunks = parse_ts(code, &test_source(), "math", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.math.add");
        assert!(
            chunks[0]
                .signature
                .as_deref()
                .unwrap()
                .contains("const add")
        );
    }

    #[test]
    fn test_skip_non_exported() {
        let code = r#"
/** Private helper. */
function helper() {
    return 42;
}

/** Public API. */
export function publicFn(): number {
    return helper();
}
"#;
        let chunks = parse_ts(code, &test_source(), "mod", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.mod.publicFn");
    }

    #[test]
    fn test_skip_undocumented() {
        let code = r#"
export function noDoc(): void {
    // no jsdoc
}

/** Has docs. */
export function withDoc(): void {}
"#;
        let chunks = parse_ts(code, &test_source(), "mod", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "mylib.mod.withDoc");
    }

    #[test]
    fn test_multiline_jsdoc() {
        let code = r#"
/**
 * Process the input data.
 *
 * @param data - The input data to process.
 * @returns The processed result.
 */
export function process(data: string): string {
    return data.trim();
}
"#;
        let chunks = parse_ts(code, &test_source(), "mod", "typescript");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].doc.contains("Process the input data"));
        assert!(chunks[0].doc.contains("@param data"));
        assert!(chunks[0].doc.contains("@returns"));
    }

    #[test]
    fn test_empty_file() {
        let chunks = parse_ts("", &test_source(), "mod", "typescript");
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_index_module_uses_package_name() {
        let code = r#"
/** Setup the library. */
export function setup(): void {}
"#;
        let chunks = parse_ts(code, &test_source(), "index", "typescript");
        assert_eq!(chunks[0].qualified_name, "mylib.setup");
    }

    #[test]
    fn test_can_handle_ts_and_js() {
        let ext = TypeScriptExtractor;

        let ts_source = Source {
            name: "lib".to_string(),
            version: None,
            kind: SourceKind::File(std::path::PathBuf::from("main.ts")),
            language: Some("typescript".to_string()),
        };
        assert!(ext.can_handle(&ts_source));

        let js_source = Source {
            name: "lib".to_string(),
            version: None,
            kind: SourceKind::File(std::path::PathBuf::from("main.js")),
            language: Some("javascript".to_string()),
        };
        assert!(ext.can_handle(&js_source));

        let py_source = Source {
            name: "lib".to_string(),
            version: None,
            kind: SourceKind::File(std::path::PathBuf::from("main.py")),
            language: Some("python".to_string()),
        };
        assert!(!ext.can_handle(&py_source));
    }

    #[test]
    fn test_export_default_class() {
        let code = r#"
/** The app component. */
export default class App {
    render() {}
}
"#;
        let chunks = parse_ts(code, &test_source(), "mod", "typescript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "class");
        assert_eq!(chunks[0].qualified_name, "mylib.mod.App");
    }

    #[test]
    fn test_javascript_language_tag() {
        let code = r#"
/** Say hi. */
export function hi() {
    return "hi";
}
"#;
        let chunks = parse_ts(code, &test_source(), "mod", "javascript");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].language, "javascript");
    }

    #[test]
    fn test_extract_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("utils");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(
            sub.join("helpers.ts"),
            "/** A helper. */\nexport function help(): void {}\n",
        )
        .unwrap();

        let source = Source {
            name: "mylib".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::LocalPath(dir.path().to_path_buf()),
            language: Some("typescript".to_string()),
        };
        let ext = TypeScriptExtractor;
        let chunks = ext.extract(&source).unwrap();
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].qualified_name.contains("utils.helpers.help"));
    }

    #[test]
    fn test_skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let nm = dir.path().join("node_modules");
        std::fs::create_dir(&nm).unwrap();
        std::fs::write(
            nm.join("dep.ts"),
            "/** Dep. */\nexport function dep(): void {}\n",
        )
        .unwrap();

        let source = Source {
            name: "mylib".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::LocalPath(dir.path().to_path_buf()),
            language: Some("typescript".to_string()),
        };
        let ext = TypeScriptExtractor;
        let chunks = ext.extract(&source).unwrap();
        assert!(chunks.is_empty());
    }
}
