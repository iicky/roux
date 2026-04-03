use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Extractor, RawChunk};
use crate::source::{Source, SourceKind};

pub struct RustdocExtractor;

impl Extractor for RustdocExtractor {
    fn can_handle(&self, source: &Source) -> bool {
        matches!(source.kind, SourceKind::Crate(_))
            || source.format_hint() == Some("rustdoc")
            || source.detected_language() == Some("rust")
    }

    fn extract(&self, source: &Source) -> Result<Vec<RawChunk>> {
        match &source.kind {
            SourceKind::Crate(name) => {
                let version = source.version.as_deref().unwrap_or("latest");
                let dir = download_crate(name, version)?;
                extract_from_dir(&dir, source)
            }
            SourceKind::LocalPath(path) => extract_from_dir(path, source),
            SourceKind::File(path) => {
                let code = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let module = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                parse_rust_source(&code, source, &module)
            }
            SourceKind::Url(_) => anyhow::bail!("RustdocExtractor does not support URLs"),
        }
    }
}

/// Download a crate from crates.io and extract to a temp directory.
fn validate_crate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        anyhow::bail!("invalid crate name: {name:?} (must match [a-zA-Z0-9_-]+)");
    }
    Ok(())
}

fn download_crate(name: &str, version: &str) -> Result<PathBuf> {
    validate_crate_name(name)?;
    let url = if version == "latest" {
        // First fetch the latest version number
        let meta_url = format!("https://crates.io/api/v1/crates/{name}");
        let client = reqwest::blocking::Client::builder()
            .user_agent("roux-cli/0.0.1")
            .build()?;
        let meta: serde_json::Value = client.get(&meta_url).send()?.json()?;
        let ver = meta["crate"]["max_stable_version"]
            .as_str()
            .or_else(|| meta["crate"]["max_version"].as_str())
            .context("could not determine latest version")?;
        format!("https://crates.io/api/v1/crates/{name}/{ver}/download")
    } else {
        format!("https://crates.io/api/v1/crates/{name}/{version}/download")
    };

    eprintln!("Downloading {name}...");
    let client = reqwest::blocking::Client::builder()
        .user_agent("roux-cli/0.0.1")
        .build()?;
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("downloading crate {name}"))?;

    if !response.status().is_success() {
        anyhow::bail!("failed to download {name}: HTTP {}", response.status());
    }

    let bytes = response.bytes()?;

    // Extract .tar.gz to a random temp directory (auto-cleaned on drop)
    let tmp_dir = tempfile::tempdir().context("creating temp directory")?;

    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);

    // Validate each entry to prevent path traversal attacks (e.g. ../../../etc/passwd)
    let canonical_tmp = tmp_dir.path().canonicalize()?;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?;
        if path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
            || path.is_absolute()
        {
            anyhow::bail!(
                "refusing to extract tar entry with unsafe path: {}",
                path.display()
            );
        }
        let dest = canonical_tmp.join(&path);
        entry.unpack(&dest)?;
    }

    // The extracted dir is usually {name}-{version}/
    // Persist the tempdir so it's not cleaned up when we return the path
    let tmp_path = tmp_dir.keep();
    let entries: Vec<_> = std::fs::read_dir(&tmp_path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();

    if let Some(entry) = entries.first() {
        Ok(entry.path())
    } else {
        Ok(tmp_path)
    }
}

/// Walk a directory for .rs files and extract chunks from each.
fn extract_from_dir(dir: &Path, source: &Source) -> Result<Vec<RawChunk>> {
    let mut chunks = Vec::new();

    // Find the src/ directory if it exists, otherwise use root
    let src_dir = if dir.join("src").is_dir() {
        dir.join("src")
    } else {
        dir.to_path_buf()
    };

    walk_rs_files(&src_dir, source, &src_dir, &mut chunks, 0)?;
    Ok(chunks)
}

fn walk_rs_files(
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
            walk_rs_files(&path, source, base, chunks, depth + 1)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let code = match super::safe_read_file(&path) {
                Some(c) => c,
                None => continue,
            };

            // Build module path from file path relative to base
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let module = rel
                .with_extension("")
                .to_string_lossy()
                .replace(['/', '\\'], "::")
                .replace("::mod", "");

            if let Ok(mut file_chunks) = parse_rust_source(&code, source, &module) {
                chunks.append(&mut file_chunks);
            }
        }
    }

    Ok(())
}

/// Parse a single Rust source file and extract public items as RawChunks.
fn parse_rust_source(code: &str, source: &Source, module: &str) -> Result<Vec<RawChunk>> {
    let file = syn::parse_file(code).context("parsing Rust source")?;
    let mut chunks = Vec::new();
    let source_name = &source.name;
    let source_version = source.version.as_deref().unwrap_or("unknown");
    let prefix = if module.is_empty() || module == "lib" {
        source_name.clone()
    } else {
        format!("{source_name}::{module}")
    };

    for item in &file.items {
        extract_item(item, &prefix, source_name, source_version, &mut chunks);
    }

    Ok(chunks)
}

fn extract_item(
    item: &syn::Item,
    prefix: &str,
    source_name: &str,
    source_version: &str,
    chunks: &mut Vec<RawChunk>,
) {
    match item {
        syn::Item::Fn(f) => {
            if !is_public(&f.vis) {
                return;
            }
            let name = f.sig.ident.to_string();
            let qualified = format!("{prefix}::{name}");
            let sig = fn_signature(&f.sig);
            let doc = extract_doc_attrs(&f.attrs);
            let doc_or_sig = if doc.is_empty() { &sig } else { &doc };
            let body = RawChunk::build_body("function", &qualified, Some(&sig), doc_or_sig);
            chunks.push(RawChunk {
                source_name: source_name.to_string(),
                source_version: source_version.to_string(),
                language: "rust".to_string(),
                item_type: "function".to_string(),
                qualified_name: qualified,
                signature: Some(sig),
                doc: doc.to_string(),
                body,
                url: None,
            });
        }
        syn::Item::Struct(s) => {
            if !is_public(&s.vis) {
                return;
            }
            let name = s.ident.to_string();
            let qualified = format!("{prefix}::{name}");
            let doc = extract_doc_attrs(&s.attrs);
            let generics = generics_to_string(&s.generics);
            let sig = format!("struct {name}{generics}");
            let doc_or_sig = if doc.is_empty() { &sig } else { &doc };
            let body = RawChunk::build_body("struct", &qualified, Some(&sig), doc_or_sig);
            chunks.push(RawChunk {
                source_name: source_name.to_string(),
                source_version: source_version.to_string(),
                language: "rust".to_string(),
                item_type: "struct".to_string(),
                qualified_name: qualified,
                signature: Some(sig),
                doc,
                body,
                url: None,
            });
        }
        syn::Item::Enum(e) => {
            if !is_public(&e.vis) {
                return;
            }
            let name = e.ident.to_string();
            let qualified = format!("{prefix}::{name}");
            let doc = extract_doc_attrs(&e.attrs);
            let generics = generics_to_string(&e.generics);
            let sig = format!("enum {name}{generics}");
            let doc_or_sig = if doc.is_empty() { &sig } else { &doc };
            let body = RawChunk::build_body("enum", &qualified, Some(&sig), doc_or_sig);
            chunks.push(RawChunk {
                source_name: source_name.to_string(),
                source_version: source_version.to_string(),
                language: "rust".to_string(),
                item_type: "enum".to_string(),
                qualified_name: qualified,
                signature: Some(sig),
                doc,
                body,
                url: None,
            });
        }
        syn::Item::Trait(t) => {
            if !is_public(&t.vis) {
                return;
            }
            let name = t.ident.to_string();
            let qualified = format!("{prefix}::{name}");
            let doc = extract_doc_attrs(&t.attrs);
            let generics = generics_to_string(&t.generics);
            let bounds = if t.supertraits.is_empty() {
                String::new()
            } else {
                let bounds_str: Vec<String> = t
                    .supertraits
                    .iter()
                    .map(|b| quote::quote!(#b).to_string())
                    .collect();
                format!(": {}", bounds_str.join(" + "))
            };
            let sig = format!("trait {name}{generics}{bounds}");
            let doc_or_sig = if doc.is_empty() { &sig } else { &doc };
            let body = RawChunk::build_body("trait", &qualified, Some(&sig), doc_or_sig);
            chunks.push(RawChunk {
                source_name: source_name.to_string(),
                source_version: source_version.to_string(),
                language: "rust".to_string(),
                item_type: "trait".to_string(),
                qualified_name: qualified.clone(),
                signature: Some(sig),
                doc,
                body,
                url: None,
            });

            // Also extract trait methods
            for trait_item in &t.items {
                if let syn::TraitItem::Fn(method) = trait_item {
                    let method_name = method.sig.ident.to_string();
                    let method_qualified = format!("{qualified}::{method_name}");
                    let method_sig = fn_signature(&method.sig);
                    let method_doc = extract_doc_attrs(&method.attrs);
                    let method_doc_or_sig = if method_doc.is_empty() {
                        &method_sig
                    } else {
                        &method_doc
                    };
                    let body = RawChunk::build_body(
                        "method",
                        &method_qualified,
                        Some(&method_sig),
                        method_doc_or_sig,
                    );
                    chunks.push(RawChunk {
                        source_name: source_name.to_string(),
                        source_version: source_version.to_string(),
                        language: "rust".to_string(),
                        item_type: "method".to_string(),
                        qualified_name: method_qualified,
                        signature: Some(method_sig),
                        doc: method_doc,
                        body,
                        url: None,
                    });
                }
            }
        }
        syn::Item::Impl(imp) => {
            // Extract methods from impl blocks
            let type_name = type_to_string(&imp.self_ty);
            let impl_prefix = format!("{prefix}::{type_name}");

            for impl_item in &imp.items {
                if let syn::ImplItem::Fn(method) = impl_item {
                    if !is_public(&method.vis) {
                        continue;
                    }
                    let method_name = method.sig.ident.to_string();
                    let method_qualified = format!("{impl_prefix}::{method_name}");
                    let method_sig = fn_signature(&method.sig);
                    let method_doc = extract_doc_attrs(&method.attrs);
                    let method_doc_or_sig = if method_doc.is_empty() {
                        &method_sig
                    } else {
                        &method_doc
                    };
                    let body = RawChunk::build_body(
                        "method",
                        &method_qualified,
                        Some(&method_sig),
                        method_doc_or_sig,
                    );
                    chunks.push(RawChunk {
                        source_name: source_name.to_string(),
                        source_version: source_version.to_string(),
                        language: "rust".to_string(),
                        item_type: "method".to_string(),
                        qualified_name: method_qualified,
                        signature: Some(method_sig),
                        doc: method_doc,
                        body,
                        url: None,
                    });
                }
            }
        }
        syn::Item::Mod(m) => {
            if !is_public(&m.vis) {
                return;
            }
            let mod_name = m.ident.to_string();
            let mod_prefix = format!("{prefix}::{mod_name}");

            // Extract doc on the module itself
            let doc = extract_doc_attrs(&m.attrs);
            if !doc.is_empty() {
                let body = RawChunk::build_body("module", &mod_prefix, None, &doc);
                chunks.push(RawChunk {
                    source_name: source_name.to_string(),
                    source_version: source_version.to_string(),
                    language: "rust".to_string(),
                    item_type: "module".to_string(),
                    qualified_name: mod_prefix.clone(),
                    signature: None,
                    doc,
                    body,
                    url: None,
                });
            }

            // Recurse into inline module items
            if let Some((_, items)) = &m.content {
                for sub_item in items {
                    extract_item(sub_item, &mod_prefix, source_name, source_version, chunks);
                }
            }
        }
        syn::Item::Type(t) => {
            if !is_public(&t.vis) {
                return;
            }
            let name = t.ident.to_string();
            let qualified = format!("{prefix}::{name}");
            let doc = extract_doc_attrs(&t.attrs);
            let generics = generics_to_string(&t.generics);
            let sig = format!("type {name}{generics}");
            let doc_or_sig = if doc.is_empty() { &sig } else { &doc };
            let body = RawChunk::build_body("type", &qualified, Some(&sig), doc_or_sig);
            chunks.push(RawChunk {
                source_name: source_name.to_string(),
                source_version: source_version.to_string(),
                language: "rust".to_string(),
                item_type: "type".to_string(),
                qualified_name: qualified,
                signature: Some(sig),
                doc,
                body,
                url: None,
            });
        }
        syn::Item::Const(c) => {
            if !is_public(&c.vis) {
                return;
            }
            let name = c.ident.to_string();
            let qualified = format!("{prefix}::{name}");
            let doc = extract_doc_attrs(&c.attrs);
            let sig = format!("const {name}");
            let doc_or_sig = if doc.is_empty() { &sig } else { &doc };
            let body = RawChunk::build_body("const", &qualified, Some(&sig), doc_or_sig);
            chunks.push(RawChunk {
                source_name: source_name.to_string(),
                source_version: source_version.to_string(),
                language: "rust".to_string(),
                item_type: "const".to_string(),
                qualified_name: qualified,
                signature: Some(sig),
                doc,
                body,
                url: None,
            });
        }
        _ => {}
    }
}

/// Convert generics (including where clauses) to a string.
fn generics_to_string(generics: &syn::Generics) -> String {
    if generics.params.is_empty() && generics.where_clause.is_none() {
        return String::new();
    }
    let tokens = quote::quote!(#generics);
    tokens.to_string()
}

fn is_public(vis: &syn::Visibility) -> bool {
    matches!(vis, syn::Visibility::Public(_))
}

/// Extract doc comments from attributes (both `///` and `#[doc = "..."]`).
fn extract_doc_attrs(attrs: &[syn::Attribute]) -> String {
    let mut docs = Vec::new();
    for attr in attrs {
        if attr.path().is_ident("doc")
            && let syn::Meta::NameValue(nv) = &attr.meta
            && let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
        {
            docs.push(s.value());
        }
    }
    let result = docs.join("\n");
    result.trim().to_string()
}

/// Convert a function signature to a string.
fn fn_signature(sig: &syn::Signature) -> String {
    let tokens = quote::quote!(#sig);
    tokens.to_string()
}

/// Convert a type to a simple string representation.
fn type_to_string(ty: &syn::Type) -> String {
    let tokens = quote::quote!(#ty);
    // Clean up spacing
    tokens
        .to_string()
        .replace(" :: ", "::")
        .replace("< ", "<")
        .replace(" >", ">")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Source;

    fn test_source() -> Source {
        Source {
            name: "test".to_string(),
            version: Some("1.0.0".to_string()),
            kind: SourceKind::Crate("test".to_string()),
            language: Some("rust".to_string()),
        }
    }

    #[test]
    fn test_extract_public_function() {
        let code = r#"
/// Does something useful.
pub fn do_thing(x: i32) -> bool {
    true
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "function");
        assert_eq!(chunks[0].qualified_name, "test::mymod::do_thing");
        assert!(
            chunks[0]
                .signature
                .as_ref()
                .unwrap()
                .contains("fn do_thing")
        );
        assert!(chunks[0].doc.contains("Does something useful"));
    }

    #[test]
    fn test_skip_private_items() {
        let code = r#"
/// Private function.
fn private_fn() {}

/// Public function.
pub fn public_fn() {}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].qualified_name, "test::mymod::public_fn");
    }

    #[test]
    fn test_undocumented_items_included() {
        let code = r#"
pub fn no_docs() {}

/// Has docs.
pub fn has_docs() {}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 2);
        // Undocumented item uses signature as body
        let no_docs = chunks
            .iter()
            .find(|c| c.qualified_name == "test::mymod::no_docs")
            .unwrap();
        assert!(no_docs.doc.is_empty());
        assert!(no_docs.body.contains("fn no_docs"));
    }

    #[test]
    fn test_extract_struct() {
        let code = r#"
/// A useful struct.
pub struct MyStruct {
    pub field: i32,
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "struct");
        assert_eq!(chunks[0].qualified_name, "test::mymod::MyStruct");
    }

    #[test]
    fn test_extract_trait_with_methods() {
        let code = r#"
/// A trait for things.
pub trait MyTrait {
    /// Do the thing.
    fn do_it(&self);
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].item_type, "trait");
        assert_eq!(chunks[1].item_type, "method");
        assert_eq!(chunks[1].qualified_name, "test::mymod::MyTrait::do_it");
    }

    #[test]
    fn test_trait_bounds_preserved() {
        let code = r#"
/// A bounded trait.
pub trait Sendable: Send + Sync {
    /// Required method.
    fn process(&self);
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        let trait_chunk = chunks.iter().find(|c| c.item_type == "trait").unwrap();
        let sig = trait_chunk.signature.as_deref().unwrap();
        assert!(sig.contains("Send"), "signature should contain Send: {sig}");
        assert!(sig.contains("Sync"), "signature should contain Sync: {sig}");
    }

    #[test]
    fn test_generics_preserved() {
        let code = r#"
/// A generic struct.
pub struct Container<T: Clone> {
    inner: T,
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        let sig = chunks[0].signature.as_deref().unwrap();
        assert!(
            sig.contains("<"),
            "signature should contain generics: {sig}"
        );
    }

    #[test]
    fn test_extract_impl_methods() {
        let code = r#"
pub struct Foo;

impl Foo {
    /// Creates a new Foo.
    pub fn new() -> Self {
        Foo
    }
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 2); // struct Foo (undocumented) + method new
        let method = chunks.iter().find(|c| c.item_type == "method").unwrap();
        assert_eq!(method.qualified_name, "test::mymod::Foo::new");
    }

    #[test]
    fn test_extract_enum() {
        let code = r#"
/// An important enum.
pub enum Color {
    Red,
    Blue,
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].item_type, "enum");
    }

    #[test]
    fn test_extract_module() {
        let code = r#"
/// Submodule docs.
pub mod sub {
    /// Inner function.
    pub fn inner() {}
}
"#;
        let chunks = parse_rust_source(code, &test_source(), "mymod").unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].item_type, "module");
        assert_eq!(chunks[0].qualified_name, "test::mymod::sub");
        assert_eq!(chunks[1].item_type, "function");
        assert_eq!(chunks[1].qualified_name, "test::mymod::sub::inner");
    }

    #[test]
    fn test_lib_module_uses_crate_name() {
        let code = r#"
/// A function at crate root.
pub fn root_fn() {}
"#;
        let chunks = parse_rust_source(code, &test_source(), "lib").unwrap();
        assert_eq!(chunks[0].qualified_name, "test::root_fn");
    }

    #[test]
    #[ignore] // requires network
    fn test_download_and_extract_crate() {
        let source = Source {
            name: "anyhow".to_string(),
            version: Some("1.0.102".to_string()),
            kind: SourceKind::Crate("anyhow".to_string()),
            language: Some("rust".to_string()),
        };

        let extractor = RustdocExtractor;
        let chunks = extractor.extract(&source).unwrap();
        assert!(!chunks.is_empty(), "should extract chunks from anyhow");

        // Should find some well-known items
        let has_context = chunks.iter().any(|c| c.qualified_name.contains("Context"));
        assert!(has_context, "should find Context trait in anyhow");
    }

    #[test]
    fn test_validate_crate_name() {
        assert!(validate_crate_name("tokio").is_ok());
        assert!(validate_crate_name("serde-json").is_ok());
        assert!(validate_crate_name("my_crate").is_ok());
        assert!(validate_crate_name("").is_err());
        assert!(validate_crate_name("../evil").is_err());
        assert!(validate_crate_name("foo bar").is_err());
        assert!(validate_crate_name("name;rm -rf /").is_err());
    }

    #[test]
    fn test_symlinks_skipped_in_walk() {
        let dir = tempfile::tempdir().unwrap();
        let real_file = dir.path().join("real.rs");
        std::fs::write(&real_file, "/// Doc.\npub fn real() {}").unwrap();

        #[cfg(unix)]
        {
            let link = dir.path().join("link.rs");
            std::os::unix::fs::symlink(&real_file, &link).unwrap();

            let source = test_source();
            let mut chunks = Vec::new();
            walk_rs_files(dir.path(), &source, dir.path(), &mut chunks, 0).unwrap();
            // Should only get the real file, not the symlink
            assert_eq!(chunks.len(), 1);
        }
    }
}
