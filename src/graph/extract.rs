use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Language, Node as TsNode, Parser};

use super::{Edge, Node};

/// Per-file manifest entry: what was indexed and a cheap change-detection key.
/// `content_hash` is the authoritative change signal (the file node already
/// computes it); `mtime` is a cheap pre-filter to skip hashing unchanged files.
#[derive(Debug, Clone, PartialEq)]
pub struct FileMeta {
    /// Path relative to the source root.
    pub path: String,
    pub content_hash: String,
    /// Unix seconds; `None` when the filesystem didn't report it.
    pub mtime: Option<i64>,
}

/// Extraction result from a directory or single file.
pub struct FileGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// One entry per file that was read and indexed (roux-vmdf).
    pub files: Vec<FileMeta>,
}

/// Run the global finalize pipeline over a fully-collected graph: dedup nodes,
/// resolve cross-file references (from persisted ref_names), infer convention
/// edges (test/override/export), and generate NL descriptions. Shared by
/// `extract_dir` and the incremental re-extraction path so both produce an
/// identical graph from the same node/edge set.
pub(crate) fn finalize_graph(nodes: &mut Vec<Node>, edges: &mut Vec<Edge>) {
    merge_duplicate_nodes(nodes);
    // Canonical node order: `resolve_references` resolves an ambiguous name to
    // the FIRST matching node, and the passes below iterate nodes — so a stable
    // (file_path, start_line, id) order makes finalize a pure function of the
    // node/edge SETS, independent of collection or DB-load order. It also
    // matches the order the store reads nodes back in.
    nodes.sort_by(|a, b| {
        (a.file_path.as_str(), a.start_line, a.id.as_str()).cmp(&(
            b.file_path.as_str(),
            b.start_line,
            b.id.as_str(),
        ))
    });
    resolve_references(edges, nodes);
    infer_test_edges(nodes, edges);
    infer_override_edges(nodes, edges);
    infer_export_edges(nodes, edges);
    // Drop edges with no origin node. A reference sitting outside any symbol in
    // a file whose file node couldn't be built leaves from_id empty; such an
    // edge has no traversable source, never resolves (resolution only rewrites
    // to_id), and would seed the empty-string id into graph traversal. Remove it
    // before the edge set is deduped, described, and persisted.
    edges.retain(|e| !e.from_id.is_empty());
    dedup_edges(edges);
    generate_descriptions(nodes, edges);
}

/// Canonicalize edge order and collapse duplicates by (from_id, to_id, kind),
/// keeping the first survivor after a full-tuple sort. Several extractors can
/// emit the same edge (e.g. a tags @reference and an AST-walked call), and the
/// store's PK dedups on insert — dedup here too so the in-memory graph matches
/// what's stored. Sorting first makes both the survivor (for a rare same-key /
/// different-ref_name collision) and the final order deterministic, so
/// `generate_descriptions` (which reads the first few neighbors per node) and
/// the stored graph are a pure function of the edge SET.
fn dedup_edges(edges: &mut Vec<Edge>) {
    edges.sort_by(|a, b| {
        (&a.from_id, &a.to_id, &a.kind, &a.ref_name).cmp(&(
            &b.from_id,
            &b.to_id,
            &b.kind,
            &b.ref_name,
        ))
    });
    let mut seen = std::collections::HashSet::new();
    edges.retain(|e| seen.insert((e.from_id.clone(), e.to_id.clone(), e.kind.clone())));
}

/// Extract nodes and edges from a source directory.
pub fn extract_dir(
    dir: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
) -> Result<FileGraph> {
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();

    let mut stats = WalkStats::default();
    walk_dir(
        dir,
        dir,
        source_name,
        source_version,
        language_hint,
        &mut all_nodes,
        &mut all_edges,
        0,
        &mut stats,
    )?;
    if stats.read_errors > 0 || stats.parse_errors > 0 {
        eprintln!(
            "  skipped {} unreadable file(s), {} failed to parse",
            stats.read_errors, stats.parse_errors
        );
    }

    finalize_graph(&mut all_nodes, &mut all_edges);

    Ok(FileGraph {
        nodes: all_nodes,
        edges: all_edges,
        files: std::mem::take(&mut stats.files),
    })
}

/// Extract nodes and edges from a single file.
pub fn extract_file(
    path: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
) -> Result<FileGraph> {
    let lang = language_hint
        .or_else(|| detect_language(path))
        .context("cannot detect language")?;

    let ts_lang = get_ts_language(lang).with_context(|| format!("unsupported language: {lang}"))?;

    let code =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let rel_path = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // Create file node
    let file_qualified = format!("{source_name}::{rel_path}");
    let file_id = Node::id_for(source_name, &file_qualified);
    let file_hash = blake3::hash(code.as_bytes()).to_hex().to_string();
    let file_lines = code.lines().count();
    nodes.push(make_file_node(
        &file_id,
        &rel_path,
        &file_qualified,
        source_name,
        lang,
        &rel_path,
        Some(&file_hash),
        file_lines,
    ));

    extract_from_source(
        &code,
        ts_lang,
        lang,
        &rel_path,
        source_name,
        source_version,
        &mut nodes,
        &mut edges,
        Some(&file_id),
    )?;

    merge_duplicate_nodes(&mut nodes);

    let files = vec![FileMeta {
        path: rel_path,
        content_hash: file_hash,
        mtime: entry_mtime(path),
    }];
    Ok(FileGraph {
        nodes,
        edges,
        files,
    })
}

/// Re-extract only the files whose content changed since `prior`, keep every
/// unchanged file's nodes and edges verbatim, then run the global finalize pass
/// over the combined set. Finalize re-resolves references across the FULL
/// current node set and re-infers convention edges, so a symbol renamed, added,
/// or removed in one file correctly rewires references from every other file.
/// The result is the same node and edge set as a full [`extract_dir`] of the
/// current tree, at the cost of re-parsing only the touched files.
///
/// `prior` is the previously-extracted graph; its `files` manifest is the change
/// baseline. Nodes are reconstructed in current-manifest order — the same
/// deterministic order a full walk produces — because [`resolve_references`]
/// picks the first matching node for an ambiguous reference, making node order a
/// correctness input to the resolved edge set.
pub fn reextract_incremental(
    dir: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
    prior: &FileGraph,
) -> Result<FileGraph> {
    use std::collections::{HashMap, HashSet};

    // Current manifest (read + hash only), in the deterministic walk order that
    // `walk_dir` also uses.
    let current = list_source_files(dir, language_hint)?;

    let prior_hashes: HashMap<&str, &str> = prior
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.content_hash.as_str()))
        .collect();

    // A file is unchanged iff it is present in the prior manifest with the same
    // content hash. Everything else (new or modified) is re-parsed; files only
    // in `prior` (deleted) simply never contribute kept nodes below.
    let unchanged: HashSet<&str> = current
        .iter()
        .filter(|f| prior_hashes.get(f.path.as_str()) == Some(&f.content_hash.as_str()))
        .map(|f| f.path.as_str())
        .collect();

    // Map each prior node id to its file, and group unchanged files' prior nodes
    // by path (preserving each file's original relative order — prior.nodes is
    // already in walk order).
    let mut node_file: HashMap<&str, &str> = HashMap::with_capacity(prior.nodes.len());
    let mut kept_by_file: HashMap<&str, Vec<Node>> = HashMap::new();
    for n in &prior.nodes {
        node_file.insert(n.id.as_str(), n.file_path.as_str());
        if unchanged.contains(n.file_path.as_str()) {
            kept_by_file
                .entry(n.file_path.as_str())
                .or_default()
                .push(n.clone());
        }
    }

    // Group unchanged files' non-inferred edges by source file, preserving each
    // file's original relative edge order. Convention edges (tests/overrides/
    // exports) are dropped — finalize regenerates them over the full node set.
    // Edge order is load-bearing: generate_descriptions reads the first few
    // callees/callers per node, so a full walk's file-contiguous edge order must
    // be reproduced, not merely its edge set.
    let mut kept_edges_by_file: HashMap<&str, Vec<Edge>> = HashMap::new();
    for e in &prior.edges {
        if matches!(e.kind.as_str(), "tests" | "overrides" | "exports") {
            continue;
        }
        if let Some(&file) = node_file.get(e.from_id.as_str())
            && unchanged.contains(file)
        {
            kept_edges_by_file.entry(file).or_default().push(e.clone());
        }
    }

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut stats = WalkStats::default();

    // Rebuild nodes and edges in current-manifest order, splicing each file's
    // kept nodes+edges or freshly parsed nodes+edges into place. The combined
    // order equals a full walk's, so reference resolution and description
    // generation are identical.
    for f in &current {
        match kept_by_file.remove(f.path.as_str()) {
            Some(kept) => {
                nodes.extend(kept);
                if let Some(kept_e) = kept_edges_by_file.remove(f.path.as_str()) {
                    edges.extend(kept_e);
                }
            }
            None => extract_indexed_file(
                &dir.join(&f.path),
                &f.path,
                source_name,
                source_version,
                language_hint,
                &mut nodes,
                &mut edges,
                &mut stats,
            ),
        }
    }

    finalize_graph(&mut nodes, &mut edges);

    Ok(FileGraph {
        nodes,
        edges,
        files: current,
    })
}

/// Walk a source tree and return the file manifest WITHOUT parsing — read +
/// hash only (roux-vmdf). Applies the same skip/inclusion rules as `extract_dir`
/// (guarded by `manifest_walk_matches_extraction`) so its output can be diffed
/// against a stored manifest to find changed files cheaply.
pub fn list_source_files(dir: &Path, language_hint: Option<&str>) -> Result<Vec<FileMeta>> {
    let mut out = Vec::new();
    list_source_files_inner(dir, dir, language_hint, 0, &mut out);
    Ok(out)
}

fn list_source_files_inner(
    dir: &Path,
    base: &Path,
    language_hint: Option<&str>,
    depth: usize,
    out: &mut Vec<FileMeta>,
) {
    if depth > 100 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    // Sort by name to match `walk_dir`'s deterministic order (see the note
    // there); the two walks must agree so the incremental manifest lines up
    // with full extraction.
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_skipped_entry_name(name)
        {
            continue;
        }
        if path.is_dir() {
            list_source_files_inner(&path, base, language_hint, depth + 1, out);
            continue;
        }
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            > crate::settings::get().max_file_bytes as u64
        {
            continue;
        }
        if !indexable_file(&path, language_hint) {
            continue;
        }
        // Read to hash. An unreadable file is skipped — extraction would skip it
        // too (it wouldn't be in the manifest), keeping the two walks aligned.
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rel_path = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        out.push(FileMeta {
            path: rel_path,
            content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
            mtime: entry_mtime(&path),
        });
    }
}

/// Collapse nodes that share an `id` (same `source_name::qualified_name`)
/// into a single row. Type-definition kinds (struct/enum/trait/type/union)
/// take precedence over `impl` blocks, since both shapes resolve to the
/// same qualified name but only the type definition carries the rustdoc.
/// Without this, the in-memory dedup pass that feeds INSERT OR REPLACE
/// would silently drop the struct's doc when the impl arrived second.
fn merge_duplicate_nodes(nodes: &mut Vec<Node>) {
    use std::collections::HashMap;

    fn priority(kind: &str) -> u8 {
        match kind {
            "struct" | "enum" | "trait" | "type" | "union" | "class" | "interface" => 0,
            "impl" => 1,
            _ => 2,
        }
    }

    let mut by_id: HashMap<String, usize> = HashMap::with_capacity(nodes.len());
    let mut keep: Vec<bool> = vec![true; nodes.len()];

    for i in 0..nodes.len() {
        let id = nodes[i].id.clone();
        match by_id.get(&id).copied() {
            None => {
                by_id.insert(id, i);
            }
            Some(j) => {
                let (pi, pj) = (priority(&nodes[i].kind), priority(&nodes[j].kind));
                let (winner, loser) = if pi != pj {
                    if pi < pj { (i, j) } else { (j, i) }
                } else {
                    // Same kind tier (e.g. a prototype and its definition): keep
                    // the one with the larger line span so the survivor points at
                    // the body, not the bare declaration.
                    let span = |n: &Node| n.end_line.saturating_sub(n.start_line);
                    if span(&nodes[i]) > span(&nodes[j]) { (i, j) } else { (j, i) }
                };
                if nodes[winner].doc.is_none() {
                    let doc = nodes[loser].doc.clone();
                    nodes[winner].doc = doc;
                }
                if nodes[winner].signature.is_none() {
                    let sig = nodes[loser].signature.clone();
                    nodes[winner].signature = sig;
                }
                keep[loser] = false;
                by_id.insert(nodes[winner].id.clone(), winner);
            }
        }
    }

    let mut idx = 0;
    nodes.retain(|_| {
        let k = keep[idx];
        idx += 1;
        k
    });
}

/// Files walk_dir couldn't read or parse, tallied so a partial index is
/// diagnosable rather than silently incomplete.
#[derive(Default)]
struct WalkStats {
    read_errors: usize,
    parse_errors: usize,
    /// Per-file manifest accumulated across the walk (roux-vmdf).
    files: Vec<FileMeta>,
}

/// Directory/entry names skipped by every source walk (VCS, build output,
/// vendored deps, dotfiles). Shared by `walk_dir` (extraction) and
/// `list_source_files` (cheap manifest walk) so the two can't drift.
fn is_skipped_entry_name(name: &str) -> bool {
    name.starts_with('.')
        || name == "node_modules"
        || name == "target"
        || name == "__pycache__"
        || name == "vendor"
        || name == ".git"
}

/// Whether a file would be indexed, and thus belongs in the manifest: markdown
/// docs, or a file whose language has a tree-sitter grammar. Mirrors the
/// inclusion rule inside `walk_dir`.
fn indexable_file(path: &Path, language_hint: Option<&str>) -> bool {
    let ext = path.extension().and_then(|e| e.to_str());
    if matches!(ext, Some("md" | "markdown")) {
        return true;
    }
    language_hint
        .filter(|l| get_ts_language(l).is_some())
        .or_else(|| detect_language(path))
        .is_some()
}

/// Filesystem mtime of an entry as Unix seconds, if available.
fn entry_mtime(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

fn walk_dir(
    dir: &Path,
    base: &Path,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    depth: usize,
    stats: &mut WalkStats,
) -> Result<()> {
    if depth > 100 {
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    // Sort entries by name so extraction visits files in a deterministic order
    // identical to `list_source_files` (the incremental walk). Reference
    // resolution picks the first matching node for an ambiguous name, so a
    // stable node order is a correctness input, not a cosmetic detail.
    // A single unreadable directory entry must not abort the whole walk. Tally
    // it and continue so the rest of the tree is still indexed; the cheap
    // manifest walk (`list_source_files`) drops bad entries the same way, so the
    // two walks stay aligned.
    let mut entries: Vec<_> = entries
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(_) => {
                stats.read_errors += 1;
                None
            }
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();

        // Skip symlinks, hidden dirs, node_modules, target, __pycache__
        if path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            continue;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && is_skipped_entry_name(name)
        {
            continue;
        }

        if path.is_dir() {
            walk_dir(
                &path,
                base,
                source_name,
                source_version,
                language_hint,
                nodes,
                edges,
                depth + 1,
                stats,
            )?;
            continue;
        }

        // Skip oversized files by their on-disk size, before reading the whole
        // thing into memory. (A file at exactly the limit is kept.)
        if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
            > crate::settings::get().max_file_bytes as u64
        {
            continue;
        }

        let rel_path = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        extract_indexed_file(
            &path,
            &rel_path,
            source_name,
            source_version,
            language_hint,
            nodes,
            edges,
            stats,
        );
    }

    Ok(())
}

/// Parse one already-selected file (code or markdown) into `nodes`/`edges` and
/// append its manifest entry, exactly as the directory walk does. `rel_path` is
/// the source-root-relative path used for the file-node id and symbol
/// qualification. Shared by `walk_dir` and the incremental re-extraction path so
/// both produce a byte-identical graph for the same file.
fn extract_indexed_file(
    path: &Path,
    rel_path: &str,
    source_name: &str,
    source_version: &str,
    language_hint: Option<&str>,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    stats: &mut WalkStats,
) {
    // Markdown docs: sections + backtick references, no tree-sitter grammar.
    let ext = path.extension().and_then(|e| e.to_str());
    if matches!(ext, Some("md" | "markdown")) {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                stats.read_errors += 1;
                return;
            }
        };
        stats.files.push(FileMeta {
            path: rel_path.to_string(),
            content_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
            mtime: entry_mtime(path),
        });
        extract_markdown_doc(&content, rel_path, source_name, nodes, edges);
        return;
    }

    // Use the language hint if it has a grammar, else detect from the extension.
    let lang = language_hint
        .filter(|l| get_ts_language(l).is_some())
        .or_else(|| detect_language(path));
    let lang = match lang {
        Some(l) => l,
        None => return,
    };
    let ts_lang = match get_ts_language(lang) {
        Some(l) => l,
        None => return,
    };

    let code = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            stats.read_errors += 1;
            return;
        }
    };

    // Create the file node.
    let file_qualified = format!("{source_name}::{rel_path}");
    let file_id = Node::id_for(source_name, &file_qualified);
    let file_hash = blake3::hash(code.as_bytes()).to_hex().to_string();
    let file_lines = code.lines().count();
    stats.files.push(FileMeta {
        path: rel_path.to_string(),
        content_hash: file_hash.clone(),
        mtime: entry_mtime(path),
    });
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();

    nodes.push(make_file_node(
        &file_id,
        &file_name,
        &file_qualified,
        source_name,
        lang,
        rel_path,
        Some(&file_hash),
        file_lines,
    ));

    if let Err(e) = extract_from_source(
        &code,
        ts_lang,
        lang,
        rel_path,
        source_name,
        source_version,
        nodes,
        edges,
        Some(&file_id),
    ) {
        stats.parse_errors += 1;
        eprintln!("  warning: failed to extract {rel_path}: {e}");
    }
}

/// Map a file extension to a language roux has a tree-sitter grammar for
/// (see [`get_ts_language`]). The single source of truth for code-extension
/// detection, shared with the `roux add` path.
pub(crate) fn detect_language(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("py") => Some("python"),
        Some("ts" | "tsx") => Some("typescript"),
        Some("js" | "jsx" | "mjs") => Some("javascript"),
        Some("go") => Some("go"),
        Some("cpp" | "cc" | "cxx" | "c++" | "hpp" | "hh" | "hxx" | "h" | "ino") => Some("cpp"),
        Some("c") => Some("c"),
        Some("sh" | "bash" | "zsh") => Some("bash"),
        // Only claim languages we have a tree-sitter grammar for (see
        // get_ts_language). Extensions without a grammar — .java, .rb — fall
        // through to None and are skipped, rather than being "detected" and
        // then silently dropped at parse time.
        _ => None,
    }
}

fn get_ts_language(lang: &str) -> Option<Language> {
    match lang {
        "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
        "python" => Some(tree_sitter_python::LANGUAGE.into()),
        "javascript" => Some(tree_sitter_javascript::LANGUAGE.into()),
        "typescript" | "tsx" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        "go" => Some(tree_sitter_go::LANGUAGE.into()),
        "cpp" | "c" => Some(tree_sitter_cpp::LANGUAGE.into()),
        "bash" => Some(tree_sitter_bash::LANGUAGE.into()),
        _ => None,
    }
}

/// Core extraction: parse source code and emit nodes + edges.
fn extract_from_source(
    code: &str,
    ts_lang: Language,
    lang: &str,
    file_path: &str,
    source_name: &str,
    source_version: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    file_parent_id: Option<&str>,
) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&ts_lang)
        .context("setting parser language")?;

    let tree = parser.parse(code, None).context("parsing source code")?;

    let root = tree.root_node();
    let code_bytes = code.as_bytes();

    // Extract import edges from top-level
    extract_imports(&root, code_bytes, lang, file_parent_id, edges);

    // Tags-based extraction: use tags.scm queries, fall back to AST walking
    // for languages without a tags query.
    let (tag_symbols, tag_refs) =
        super::tags::extract_tags(code_bytes, lang, ts_lang.clone(), &tree);

    if !tag_symbols.is_empty() {
        // Tags-based path: convert TaggedSymbols to Nodes, enriched via AST
        //
        // Two passes:
        // 1. Compute parent nesting from byte ranges → build qualified names + IDs
        // 2. Create nodes with correct IDs, run edge inference

        let container_kinds = [
            "class",
            "module",
            "struct",
            "enum",
            "trait",
            "impl",
            "interface",
        ];

        // Pass 1: Compute nesting to build qualified name prefixes
        struct SymMeta {
            idx: usize,
            name: String,
            kind: String,
            start_byte: usize,
            end_byte: usize,
        }
        let mut metas: Vec<SymMeta> = tag_symbols
            .iter()
            .enumerate()
            .map(|(i, s)| SymMeta {
                idx: i,
                name: s.name.clone(),
                kind: s.kind.as_str().to_string(),
                start_byte: s.start_byte,
                end_byte: s.end_byte,
            })
            .collect();
        metas.sort_by(|a, b| {
            a.start_byte
                .cmp(&b.start_byte)
                .then(b.end_byte.cmp(&a.end_byte))
        });

        // Build prefix map: sym_index → qualified prefix (e.g. "auth" for login inside mod auth)
        let mut prefix_map: std::collections::HashMap<usize, String> =
            std::collections::HashMap::new();
        let mut parent_idx_map: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        let mut stack: Vec<(usize, usize, usize, String)> = Vec::new(); // (start, end, idx, name)

        for meta in &metas {
            while let Some(top) = stack.last() {
                if meta.start_byte >= top.1 {
                    stack.pop();
                } else {
                    break;
                }
            }
            if let Some(top) = stack.last() {
                // Build full prefix from stack
                let prefix: String = stack
                    .iter()
                    .map(|(_, _, _, n)| n.as_str())
                    .collect::<Vec<_>>()
                    .join("::");
                prefix_map.insert(meta.idx, prefix);
                parent_idx_map.insert(meta.idx, top.2);
            }
            if container_kinds.contains(&meta.kind.as_str()) {
                stack.push((meta.start_byte, meta.end_byte, meta.idx, meta.name.clone()));
            }
        }

        // Pass 2: Create nodes with correct qualified names, run edge inference
        // Track IDs by sym index for parent_id lookup
        let mut id_by_idx: Vec<String> = vec![String::new(); tag_symbols.len()];

        for (i, sym) in tag_symbols.iter().enumerate() {
            let qualified = if let Some(prefix) = prefix_map.get(&i) {
                format!("{source_name}::{prefix}::{}", sym.name)
            } else {
                format!("{source_name}::{}", sym.name)
            };

            // Recover the tree-sitter AST node for enrichment
            let ts_node = root.descendant_for_byte_range(sym.start_byte, sym.end_byte);

            // Content hash from symbol source text
            let source_text = &code_bytes[sym.start_byte..sym.end_byte.min(code_bytes.len())];
            let content_hash = blake3::hash(source_text).to_hex().to_string();

            // Enrich with AST-derived metadata
            let (visibility, signature, doc) = if let Some(ref n) = ts_node {
                (
                    detect_visibility(n, code_bytes, lang),
                    extract_signature_text(n, code_bytes),
                    extract_doc_comment(n, code_bytes)
                        .or_else(|| {
                            if lang == "python" {
                                extract_python_docstring(n, code_bytes)
                            } else {
                                None
                            }
                        })
                        .or(sym.doc.clone()),
                )
            } else {
                (String::new(), None, sym.doc.clone())
            };

            // Overloads share a signatureless qualified name; the parameter list
            // separates them. Only function/method kinds overload.
            let discriminator = if matches!(sym.kind.as_str(), "function" | "method") {
                Node::param_discriminator(signature.as_deref())
            } else {
                String::new()
            };
            let id = Node::id_for_symbol(source_name, file_path, &qualified, &discriminator);
            id_by_idx[i] = id.clone();

            let parent_id = parent_idx_map
                .get(&i)
                .map(|pi| id_by_idx[*pi].clone())
                .or_else(|| file_parent_id.map(|s| s.to_string()));

            let mut node = Node {
                id: id.clone(),
                kind: sym.kind.as_str().to_string(),
                name: sym.name.clone(),
                qualified_name: qualified,
                source_name: source_name.to_string(),
                language: lang.to_string(),
                file_path: file_path.to_string(),
                start_line: sym.start_line,
                start_col: sym.start_col,
                end_line: sym.end_line,
                visibility,
                signature,
                doc,
                body: String::new(),
                parent_id,
                content_hash: Some(content_hash),
                line_count: sym.end_line.saturating_sub(sym.start_line) + 1,
                source_url: None,
                description: None,
            };
            node.body = node.build_body();

            let node_kind = node.kind.clone();
            let doc_for_refs = node.doc.clone();
            nodes.push(node);

            // Run edge inference on the AST node
            if let Some(ref n) = ts_node {
                extract_relationship_edges(n, code_bytes, lang, &id, edges);
                extract_decorator_edges(n, code_bytes, lang, &id, edges);
                if matches!(node_kind.as_str(), "function" | "method") {
                    extract_raise_edges(n, code_bytes, lang, &id, edges);
                    extract_route_registrations(n, code_bytes, lang, &id, edges);
                    extract_call_references(n, code_bytes, &id, edges);
                }
            }

            // Structured doc cross-references: author-MARKED refs in the
            // doc-comment (rustdoc `[Foo]`, Javadoc `{@link X}`, Sphinx
            // `:func:`x``, C# `cref`) resolve to `references` edges. These
            // bridge query→docstring→symbol when the target is defined
            // elsewhere — the parent edge can't reach a cross-file symbol.
            if let Some(ref d) = doc_for_refs {
                for target in extract_doc_refs(d) {
                    edges.push(Edge {
                        from_id: id.clone(),
                        to_id: format!("__unresolved::{target}"),
                        kind: "references".to_string(),
                        ref_name: None,
                    });
                }
            }
        }

        // Convert tagged references to unresolved edges
        for r in &tag_refs {
            if !r.name.is_empty() && r.name.len() < 200 {
                // Attribute the reference to its innermost enclosing symbol so the
                // edge is a real caller->callee link with a resolvable, file-
                // attributable from_id; fall back to the file node when nothing
                // encloses it (e.g. a top-level reference).
                let from = tag_symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.start_line <= r.start_line && r.start_line <= s.end_line)
                    .min_by_key(|(_, s)| s.end_line.saturating_sub(s.start_line))
                    .map(|(i, _)| id_by_idx[i].clone())
                    .or_else(|| file_parent_id.map(|s| s.to_string()))
                    .unwrap_or_default();
                edges.push(Edge {
                    from_id: from,
                    to_id: format!("__unresolved::{}", r.name),
                    kind: match r.kind {
                        super::tags::RefKind::Call => "calls".to_string(),
                        super::tags::RefKind::Implementation => "implements".to_string(),
                    },
                    ref_name: None,
                });
            }
        }
    } else {
        // Fallback: AST-walking extraction for languages without tags.scm
        extract_node(
            &root,
            code_bytes,
            lang,
            file_path,
            source_name,
            source_version,
            nodes,
            edges,
            file_parent_id,
            "",
        );
    }

    Ok(())
}

/// Extract implements/inherits edges from class/impl/struct declarations.
fn extract_relationship_edges(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    let kind = node.kind();
    match lang {
        "rust" if kind == "impl_item" => {
            // impl Trait for Type → implements edge
            // Check for "for" keyword indicating trait impl
            let full_text = node_text(node, code);
            if full_text.contains(" for ") {
                // The trait is before "for", the type is after
                // Tree-sitter structure: impl <trait> for <type> { ... }
                let mut cursor = node.walk();
                let children: Vec<_> = node.children(&mut cursor).collect();
                // Find trait name — it's a type_identifier before the "for" keyword
                let mut found_trait = None;
                for child in &children {
                    if (child.kind() == "type_identifier"
                        || child.kind() == "generic_type"
                        || child.kind() == "scoped_type_identifier")
                        && found_trait.is_none()
                    {
                        found_trait = Some(node_text(child, code).to_string());
                    }
                }
                if let Some(trait_name) = found_trait {
                    edges.push(Edge {
                        from_id: sym_id.to_string(),
                        to_id: format!("__unresolved::{trait_name}"),
                        kind: "implements".to_string(),
                        ref_name: None,
                    });
                }
            }
        }
        "python" => {
            // class Foo(Bar, Baz): → inherits edges
            if kind == "class_definition"
                && let Some(args) = find_child_by_kind(node, "argument_list")
            {
                let mut cursor = args.walk();
                for child in args.children(&mut cursor) {
                    if child.kind() == "identifier" {
                        let parent_name = node_text(&child, code).to_string();
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{parent_name}"),
                            kind: "inherits".to_string(),
                            ref_name: None,
                        });
                    }
                }
            }
        }
        "cpp" | "c" if matches!(kind, "class_specifier" | "struct_specifier") => {
            // class Derived : public Base, virtual Mixin, ... → inherits edges.
            // The inheritance list is a `base_class_clause` child of the class
            // body's parent node, containing one or more type_identifiers
            // (optionally with access specifiers and `virtual` keyword between).
            if let Some(base_clause) = find_child_by_kind(node, "base_class_clause") {
                let mut cursor = base_clause.walk();
                for child in base_clause.children(&mut cursor) {
                    if matches!(
                        child.kind(),
                        "type_identifier" | "qualified_identifier" | "template_type"
                    ) {
                        let parent_name = node_text(&child, code).to_string();
                        // For qualified `ns::Foo`, store the leaf — resolution
                        // matches against `qualified_name` suffix anyway.
                        let leaf = parent_name
                            .rsplit("::")
                            .next()
                            .unwrap_or(&parent_name)
                            .to_string();
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{leaf}"),
                            kind: "inherits".to_string(),
                            ref_name: None,
                        });
                    }
                }
            }
        }
        "javascript" | "typescript" | "tsx" if kind == "class_declaration" => {
            // class Foo extends Bar → inherits
            // class Foo implements Bar → implements (TS only)
            if let Some(heritage) = find_child_by_kind(node, "class_heritage") {
                let text = node_text(&heritage, code);
                if text.contains("extends") {
                    // Extract the parent class name
                    if let Some(id) = find_child_by_kind(&heritage, "identifier") {
                        let parent_name = node_text(&id, code).to_string();
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{parent_name}"),
                            kind: "inherits".to_string(),
                            ref_name: None,
                        });
                    }
                }
            }
            // TypeScript implements clause
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let child_text = node_text(&child, code);
                if child_text.starts_with("implements") {
                    // Extract interface names
                    let mut inner_cursor = child.walk();
                    for inner in child.children(&mut inner_cursor) {
                        if inner.kind() == "type_identifier" || inner.kind() == "identifier" {
                            let iface_name = node_text(&inner, code).to_string();
                            edges.push(Edge {
                                from_id: sym_id.to_string(),
                                to_id: format!("__unresolved::{iface_name}"),
                                kind: "implements".to_string(),
                                ref_name: None,
                            });
                        }
                    }
                }
            }
        }
        _ => {}
    }

    // Extract type_ref edges from signatures (all languages)
    extract_type_refs(node, code, lang, sym_id, edges);
}

/// Extract type references from function parameters, return types, and field types.
fn extract_type_refs(node: &TsNode, code: &[u8], lang: &str, sym_id: &str, edges: &mut Vec<Edge>) {
    // Collect type identifiers from the node's immediate signature area
    let type_node_kinds: &[&str] = match lang {
        "rust" => &["type_identifier", "scoped_type_identifier"],
        "python" => &["type", "identifier"], // type annotations
        "javascript" | "typescript" | "tsx" => &["type_identifier", "predefined_type"],
        "go" => &["type_identifier", "qualified_type"],
        "cpp" | "c" => &["type_identifier", "qualified_identifier", "template_type"],
        _ => return,
    };

    // Only look at parameter lists and return types, not the full body
    let search_nodes: Vec<TsNode> = {
        let mut targets = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "parameters"
                | "parameter_list"
                | "formal_parameters"
                | "type_parameters"
                | "return_type"
                | "type_annotation"
                | "field_declaration_list"
                | "generic_type" => {
                    targets.push(child);
                }
                _ => {}
            }
        }
        targets
    };

    let mut seen = std::collections::HashSet::new();

    for search_node in &search_nodes {
        collect_type_refs_from(search_node, code, type_node_kinds, sym_id, edges, &mut seen);
    }
}

fn collect_type_refs_from(
    node: &TsNode,
    code: &[u8],
    type_kinds: &[&str],
    sym_id: &str,
    edges: &mut Vec<Edge>,
    seen: &mut std::collections::HashSet<String>,
) {
    if type_kinds.contains(&node.kind()) {
        let type_name = node_text(node, code).to_string();
        // Skip built-in/primitive types
        if !type_name.is_empty()
            && !is_primitive_type(&type_name)
            && type_name.len() < 200
            && seen.insert(type_name.clone())
        {
            edges.push(Edge {
                from_id: sym_id.to_string(),
                to_id: format!("__unresolved::{type_name}"),
                kind: "type_ref".to_string(),
                ref_name: None,
            });
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_type_refs_from(&child, code, type_kinds, sym_id, edges, seen);
    }
}

fn is_primitive_type(name: &str) -> bool {
    matches!(
        name,
        "str"
            | "String"
            | "string"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "bool"
            | "boolean"
            | "char"
            | "int"
            | "float"
            | "complex"
            | "None"
            | "void"
            | "undefined"
            | "null"
            | "never"
            | "any"
            | "Any"
            | "object"
            | "number"
            | "Self"
            | "self"
            | "error"
            | "byte"
            | "rune"
    )
}

/// Extract decorator edges from preceding decorator nodes.
fn extract_decorator_edges(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    match lang {
        "python" => {
            // Look for decorator siblings before the function/class
            let mut sibling = node.prev_sibling();
            while let Some(sib) = sibling {
                if sib.kind() == "decorator" {
                    let text = node_text(&sib, code);
                    let decorator_name = text
                        .trim_start_matches('@')
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !decorator_name.is_empty() {
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{decorator_name}"),
                            kind: "decorates".to_string(),
                            ref_name: None,
                        });
                        // Check for route decorators
                        if decorator_name.contains("route")
                            || decorator_name.contains(".get")
                            || decorator_name.contains(".post")
                            || decorator_name.contains(".put")
                            || decorator_name.contains(".delete")
                        {
                            let route_path = text.split(['\'', '"']).nth(1).unwrap_or("");
                            if !route_path.is_empty() {
                                edges.push(Edge {
                                    from_id: sym_id.to_string(),
                                    to_id: format!("__route::{route_path}"),
                                    kind: "routes".to_string(),
                                    ref_name: None,
                                });
                            }
                        }
                    }
                } else if sib.kind() != "comment" {
                    break;
                }
                sibling = sib.prev_sibling();
            }
        }
        "javascript" | "typescript" | "tsx" => {
            // TS/JS decorators: @Decorator before class/method
            let mut sibling = node.prev_sibling();
            while let Some(sib) = sibling {
                if sib.kind() == "decorator" {
                    let text = node_text(&sib, code);
                    let name = text
                        .trim_start_matches('@')
                        .split('(')
                        .next()
                        .unwrap_or("")
                        .trim();
                    if !name.is_empty() {
                        edges.push(Edge {
                            from_id: sym_id.to_string(),
                            to_id: format!("__unresolved::{name}"),
                            kind: "decorates".to_string(),
                            ref_name: None,
                        });
                    }
                } else {
                    break;
                }
                sibling = sib.prev_sibling();
            }
        }
        _ => {}
    }
}

/// Extract raise/throw/panic as edges from function to error type.
fn extract_raise_edges(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    let raise_kinds: &[&str] = match lang {
        "python" => &["raise_statement"],
        "javascript" | "typescript" | "tsx" => &["throw_statement"],
        "rust" => &["macro_invocation"], // bail!, panic!, anyhow!
        "go" => &["call_expression"],    // panic()
        _ => return,
    };

    let mut cursor = node.walk();
    extract_raises_recursive(node, &mut cursor, code, lang, sym_id, edges, raise_kinds);
}

fn extract_raises_recursive(
    node: &TsNode,
    _cursor: &mut tree_sitter::TreeCursor,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
    raise_kinds: &[&str],
) {
    if raise_kinds.contains(&node.kind()) {
        let text = node_text(node, code);

        let error_name = match lang {
            "python" => {
                // raise FooError(...) or raise FooError
                text.strip_prefix("raise ")
                    .and_then(|r| r.split(|c: char| c == '(' || c.is_whitespace()).next())
                    .map(|s| s.trim().to_string())
            }
            "javascript" | "typescript" | "tsx" => {
                // throw new FooError(...)
                text.strip_prefix("throw ")
                    .and_then(|r| r.strip_prefix("new "))
                    .and_then(|r| r.split('(').next())
                    .map(|s| s.trim().to_string())
            }
            "rust" => {
                // bail!(...) or panic!(...)
                let macro_name = text.split('!').next().unwrap_or("");
                if matches!(macro_name, "bail" | "panic" | "anyhow") {
                    Some(macro_name.to_string())
                } else {
                    None
                }
            }
            "go" => {
                // panic("...")
                if text.starts_with("panic(") {
                    Some("panic".to_string())
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(name) = error_name
            && !name.is_empty()
        {
            edges.push(Edge {
                from_id: sym_id.to_string(),
                to_id: format!("__unresolved::{name}"),
                kind: "raises".to_string(),
                ref_name: None,
            });
        }
        return; // Don't recurse into raise/throw children
    }

    let mut child_cursor = node.walk();
    for child in node.children(&mut child_cursor) {
        extract_raises_recursive(
            &child,
            &mut node.walk(),
            code,
            lang,
            sym_id,
            edges,
            raise_kinds,
        );
    }
}

/// Extract Go/JS route registrations: router.GET("/path", handler)
fn extract_route_registrations(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    sym_id: &str,
    edges: &mut Vec<Edge>,
) {
    if !matches!(lang, "go" | "javascript" | "typescript" | "tsx") {
        return;
    }

    // Look for method calls like router.GET("/path", handler) or app.get("/path", fn)
    let text = node_text(node, code);
    let http_methods = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "get", "post", "put", "delete", "patch",
    ];

    for method in &http_methods {
        let pattern = format!(".{method}(");
        if text.contains(&pattern) {
            // Extract the route path from the first string argument
            let route = text
                .split(&pattern)
                .nth(1)
                .and_then(|r| r.split(['\'', '"']).nth(1));
            if let Some(path) = route {
                edges.push(Edge {
                    from_id: sym_id.to_string(),
                    to_id: format!("__route::{path}"),
                    kind: "routes".to_string(),
                    ref_name: None,
                });
            }
        }
    }
}

// ─── Post-processing passes ──────────────────────────────────────────

/// Infer test edges by convention: test_foo → foo, TestFoo → Foo.
fn infer_test_edges(nodes: &[Node], edges: &mut Vec<Edge>) {
    use std::collections::HashMap;
    // First non-test node per ASCII-lowercased name, in `nodes` order. The old
    // predicate `name == n || name.eq_ignore_ascii_case(n)` reduces to a
    // case-insensitive match (an exact match implies a case-insensitive one), so
    // one first-wins map reproduces the original first-match-in-order target.
    let mut by_ci_name: HashMap<String, usize> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        if !is_test_node(n) {
            by_ci_name.entry(n.name.to_ascii_lowercase()).or_insert(i);
        }
    }

    for node in nodes {
        if !is_test_node(node) {
            continue;
        }
        if let Some(name) = extract_tested_name(&node.name)
            && let Some(&idx) = by_ci_name.get(&name.to_ascii_lowercase())
        {
            edges.push(Edge {
                from_id: node.id.clone(),
                to_id: nodes[idx].id.clone(),
                kind: "tests".to_string(),
                ref_name: None,
            });
        }
    }
}

fn is_test_node(node: &Node) -> bool {
    node.name.starts_with("test_")
        || node.name.starts_with("Test")
        || node.name.starts_with("test")
        || node.file_path.contains("test")
        || node.file_path.contains("spec")
}

fn extract_tested_name(test_name: &str) -> Option<String> {
    // test_foo → foo
    if let Some(name) = test_name.strip_prefix("test_") {
        return Some(name.to_string());
    }
    // TestFoo → Foo
    if let Some(name) = test_name.strip_prefix("Test")
        && name.starts_with(|c: char| c.is_uppercase())
    {
        return Some(name.to_string());
    }
    // testFoo → Foo (JS convention)
    if let Some(name) = test_name.strip_prefix("test")
        && name.starts_with(|c: char| c.is_uppercase())
    {
        return Some(name.to_string());
    }
    None
}

/// Infer override edges: if a child class has a method with the same name as parent.
fn infer_override_edges(nodes: &[Node], edges: &mut Vec<Edge>) {
    use std::collections::HashMap;
    // Collect inherits relationships (clone IDs to avoid borrow conflict)
    let inherits: Vec<(String, String)> = edges
        .iter()
        .filter(|e| e.kind == "inherits")
        .map(|e| (e.from_id.clone(), e.to_id.clone()))
        .collect();

    // Group method/function nodes by parent id once, preserving `nodes` order
    // within each group so same-named parent methods keep first-match semantics.
    let mut methods_by_parent: HashMap<&str, Vec<&Node>> = HashMap::new();
    for n in nodes {
        if matches!(n.kind.as_str(), "function" | "method")
            && let Some(pid) = n.parent_id.as_deref()
        {
            methods_by_parent.entry(pid).or_default().push(n);
        }
    }

    for (child_id, parent_id) in &inherits {
        let (Some(child_methods), Some(parent_methods)) = (
            methods_by_parent.get(child_id.as_str()),
            methods_by_parent.get(parent_id.as_str()),
        ) else {
            continue;
        };
        for child_method in child_methods {
            if let Some(parent_method) = parent_methods
                .iter()
                .find(|pm| pm.name == child_method.name)
            {
                edges.push(Edge {
                    from_id: child_method.id.clone(),
                    to_id: parent_method.id.clone(),
                    kind: "overrides".to_string(),
                    ref_name: None,
                });
            }
        }
    }
}

/// Infer export edges from visibility and re-export patterns.
fn infer_export_edges(nodes: &[Node], edges: &mut Vec<Edge>) {
    use std::collections::HashMap;
    // id -> node for O(1) parent lookup (ids are unique after merge).
    let by_id: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    for node in nodes {
        if node.kind == "file" {
            continue;
        }
        // Publicly visible symbols get an "exports" edge from their file.
        if matches!(node.visibility.as_str(), "pub" | "export")
            && let Some(parent_id) = node.parent_id.as_deref()
            && let Some(parent) = by_id.get(parent_id)
            && parent.kind == "file"
        {
            edges.push(Edge {
                from_id: parent.id.clone(),
                to_id: node.id.clone(),
                kind: "exports".to_string(),
                ref_name: None,
            });
        }
    }
}

/// Generic names that add no semantic signal as callers/callees.
const STOPLIST: &[&str] = &[
    "new",
    "init",
    "main",
    "run",
    "build",
    "default",
    "from",
    "into",
    "clone",
    "drop",
    "fmt",
    "eq",
    "hash",
    "cmp",
    "test",
    "setup",
    "__init__",
    "__new__",
    "__repr__",
    "__str__",
    "__eq__",
    "toString",
    "valueOf",
    "constructor",
];

/// Generate natural language descriptions from graph edges.
/// Each symbol gets a templated description like:
/// "function that calls validate_token and hash_password, called by login_handler,
///  located in auth module, implements Authenticator"
fn generate_descriptions(nodes: &mut [Node], edges: &[Edge]) {
    // Build lookup maps from immutable snapshot
    let name_map: std::collections::HashMap<String, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.name.clone()))
        .collect();
    let kind_map: std::collections::HashMap<String, String> = nodes
        .iter()
        .map(|n| (n.id.clone(), n.kind.clone()))
        .collect();
    let parent_map: std::collections::HashMap<String, (String, String)> = nodes
        .iter()
        .filter_map(|n| {
            n.parent_id.as_ref().and_then(|pid| {
                name_map.get(pid).and_then(|pname| {
                    kind_map
                        .get(pid)
                        .map(|pkind| (n.id.clone(), (pname.clone(), pkind.clone())))
                })
            })
        })
        .collect();

    // Pre-compute edge lookups
    let mut calls_out: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    let mut called_by: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    let mut implements: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    let mut inherits_from: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    let mut type_refs: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    let mut tested_by: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();
    let mut decorators: std::collections::HashMap<&str, Vec<&str>> =
        std::collections::HashMap::new();

    for edge in edges {
        match edge.kind.as_str() {
            "calls" => {
                calls_out
                    .entry(edge.from_id.as_str())
                    .or_default()
                    .push(&edge.to_id);
                called_by
                    .entry(edge.to_id.as_str())
                    .or_default()
                    .push(&edge.from_id);
            }
            "implements" => {
                implements
                    .entry(edge.from_id.as_str())
                    .or_default()
                    .push(&edge.to_id);
            }
            "inherits" => {
                inherits_from
                    .entry(edge.from_id.as_str())
                    .or_default()
                    .push(&edge.to_id);
            }
            "type_ref" => {
                type_refs
                    .entry(edge.from_id.as_str())
                    .or_default()
                    .push(&edge.to_id);
            }
            "tests" => {
                tested_by
                    .entry(edge.to_id.as_str())
                    .or_default()
                    .push(&edge.from_id);
            }
            "decorates" => {
                decorators
                    .entry(edge.from_id.as_str())
                    .or_default()
                    .push(&edge.to_id);
            }
            _ => {}
        }
    }

    for node in nodes.iter_mut() {
        if node.kind == "file" || node.kind == "doc_section" {
            continue;
        }

        let mut parts: Vec<String> = Vec::new();

        // Kind + name
        parts.push(format!("{} {}", node.kind, node.name));

        // Parent context
        if let Some((pname, pkind)) = parent_map.get(&node.id) {
            if pkind != "file" {
                parts.push(format!("in {pkind} {pname}"));
            } else {
                parts.push(format!("in {pname}"));
            }
        }

        // Calls (filtered by stoplist)
        if let Some(callees) = calls_out.get(node.id.as_str()) {
            let names: Vec<&str> = callees
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| !STOPLIST.contains(n) && n.len() > 1)
                .take(5)
                .collect();
            if !names.is_empty() {
                parts.push(format!("calls {}", names.join(" ")));
            }
        }

        // Called by (callers are semantic signal)
        if let Some(callers) = called_by.get(node.id.as_str()) {
            let names: Vec<&str> = callers
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| !STOPLIST.contains(n) && n.len() > 1)
                .take(5)
                .collect();
            if !names.is_empty() {
                parts.push(format!("called by {}", names.join(" ")));
            }
        }

        // Implements
        if let Some(traits) = implements.get(node.id.as_str()) {
            let names: Vec<&str> = traits
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .collect();
            if !names.is_empty() {
                parts.push(format!("implements {}", names.join(" ")));
            }
        }

        // Inherits
        if let Some(parents) = inherits_from.get(node.id.as_str()) {
            let names: Vec<&str> = parents
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .collect();
            if !names.is_empty() {
                parts.push(format!("extends {}", names.join(" ")));
            }
        }

        // Decorators
        if let Some(decs) = decorators.get(node.id.as_str()) {
            let names: Vec<&str> = decs
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| !STOPLIST.contains(n))
                .take(3)
                .collect();
            if !names.is_empty() {
                parts.push(format!("decorated with {}", names.join(" ")));
            }
        }

        // Type references
        if let Some(refs) = type_refs.get(node.id.as_str()) {
            let names: Vec<&str> = refs
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .filter(|n| n.len() > 2)
                .take(3)
                .collect();
            if !names.is_empty() {
                parts.push(format!("uses {}", names.join(" ")));
            }
        }

        // Tested by
        if let Some(tests) = tested_by.get(node.id.as_str()) {
            let names: Vec<&str> = tests
                .iter()
                .filter_map(|id| name_map.get(*id).map(|s| s.as_str()))
                .take(2)
                .collect();
            if !names.is_empty() {
                parts.push(format!("tested by {}", names.join(" ")));
            }
        }

        node.description = Some(parts.join(", "));
    }
}

/// Extract import/use statements as edges.
fn extract_imports(
    root: &TsNode,
    code: &[u8],
    lang: &str,
    file_node_id: Option<&str>,
    edges: &mut Vec<Edge>,
) {
    // The importing file's node id is the edge origin, so imports are attributable
    // to a file (incremental re-extraction) and re-resolvable. Empty if unavailable.
    let from = file_node_id.unwrap_or("").to_string();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        let kind = child.kind();
        match lang {
            "rust" if kind == "use_declaration" => {
                // use foo::bar::Baz;
                let text = node_text(&child, code);
                let imported = text.trim_start_matches("use ").trim_end_matches(';').trim();
                if !imported.is_empty() {
                    edges.push(Edge {
                        from_id: from.clone(),
                        to_id: format!("__unresolved::{imported}"),
                        kind: "imports".to_string(),
                        ref_name: None,
                    });
                }
            }
            "python" if kind == "import_statement" || kind == "import_from_statement" => {
                let text = node_text(&child, code);
                let imported = text
                    .trim_start_matches("from ")
                    .trim_start_matches("import ")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                if !imported.is_empty() {
                    edges.push(Edge {
                        from_id: from.clone(),
                        to_id: format!("__unresolved::{imported}"),
                        kind: "imports".to_string(),
                        ref_name: None,
                    });
                }
            }
            "javascript" | "typescript" | "tsx" if kind == "import_statement" => {
                // import { foo } from 'bar'
                if let Some(source_node) = find_child_by_kind(&child, "string") {
                    let module = node_text(&source_node, code)
                        .trim_matches(|c| c == '\'' || c == '"')
                        .to_string();
                    if !module.is_empty() {
                        edges.push(Edge {
                            from_id: from.clone(),
                            to_id: format!("__unresolved::{module}"),
                            kind: "imports".to_string(),
                            ref_name: None,
                        });
                    }
                }
            }
            "go" if kind == "import_declaration" => {
                let text = node_text(&child, code);
                for line in text.lines() {
                    let cleaned = line.trim().trim_matches('"');
                    if !cleaned.is_empty()
                        && cleaned != "import"
                        && cleaned != "("
                        && cleaned != ")"
                    {
                        edges.push(Edge {
                            from_id: from.clone(),
                            to_id: format!("__unresolved::{cleaned}"),
                            kind: "imports".to_string(),
                            ref_name: None,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// Iterative AST traversal — avoids stack overflow on deeply nested code.
fn extract_node(
    node: &TsNode,
    code: &[u8],
    lang: &str,
    file_path: &str,
    source_name: &str,
    source_version: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
    parent_id: Option<&str>,
    prefix: &str,
) {
    let kind = node.kind();

    // Try to extract a node from this node
    // Generic fallback for languages without tags.scm queries
    if let Some(mut sym) = extract_generic_node(node, code, kind) {
        // Build qualified name
        let qualified = if prefix.is_empty() {
            format!("{source_name}::{}", sym.name)
        } else {
            format!("{prefix}::{}", sym.name)
        };
        sym.qualified_name = qualified.clone();
        sym.source_name = source_name.to_string();
        sym.language = lang.to_string();
        sym.file_path = file_path.to_string();
        sym.start_line = node.start_position().row + 1;
        sym.start_col = node.start_position().column;
        sym.end_line = node.end_position().row + 1;
        sym.line_count = sym.end_line.saturating_sub(sym.start_line) + 1;
        sym.visibility = detect_visibility(node, code, lang);
        // Separate same-file overloads by parameter list.
        let discriminator = if matches!(sym.kind.as_str(), "function" | "method") {
            Node::param_discriminator(sym.signature.as_deref())
        } else {
            String::new()
        };
        sym.id = Node::id_for_symbol(source_name, file_path, &sym.qualified_name, &discriminator);
        sym.parent_id = parent_id.map(|s| s.to_string());

        // Content hash for staleness detection
        let source_text = &code[node.start_byte()..node.end_byte()];
        sym.content_hash = Some(blake3::hash(source_text).to_hex().to_string());

        sym.body = sym.build_body();

        let sym_id = sym.id.clone();
        let sym_kind = sym.kind.clone();

        nodes.push(sym);

        // Extract relationship edges
        extract_relationship_edges(node, code, lang, &sym_id, edges);
        extract_decorator_edges(node, code, lang, &sym_id, edges);

        // For functions/methods: extract raises and route registrations
        if matches!(sym_kind.as_str(), "function" | "method") {
            extract_raise_edges(node, code, lang, &sym_id, edges);
            extract_route_registrations(node, code, lang, &sym_id, edges);
        }

        // Recurse into children with this node as parent
        let new_prefix = qualified;
        let child_parent = if matches!(
            sym_kind.as_str(),
            "class" | "module" | "struct" | "enum" | "trait" | "impl"
        ) {
            Some(sym_id.as_str())
        } else {
            parent_id
        };

        // For function bodies, extract identifier references as potential call edges
        if matches!(sym_kind.as_str(), "function" | "method") {
            extract_call_references(node, code, &sym_id, edges);
        }

        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            extract_node(
                &child,
                code,
                lang,
                file_path,
                source_name,
                source_version,
                nodes,
                edges,
                child_parent,
                &new_prefix,
            );
        }
    } else {
        // Not a node node — recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            extract_node(
                &child,
                code,
                lang,
                file_path,
                source_name,
                source_version,
                nodes,
                edges,
                parent_id,
                prefix,
            );
        }
    }
}

/// Extract identifier references from a function body as potential "calls" edges.
fn extract_call_references(node: &TsNode, code: &[u8], caller_id: &str, edges: &mut Vec<Edge>) {
    let mut cursor = node.walk();
    extract_calls_recursive(node, &mut cursor, code, caller_id, edges);
}

fn extract_calls_recursive(
    node: &TsNode,
    cursor: &mut tree_sitter::TreeCursor,
    code: &[u8],
    caller_id: &str,
    edges: &mut Vec<Edge>,
) {
    // Look for call expressions
    let kind = node.kind();
    if kind == "call_expression"
        || kind == "call"
        || kind == "macro_invocation"
        || kind == "method_call_expression" // Rust: self.foo(), obj.bar()
        || kind == "member_expression"
    // JS: obj.method()
    {
        let callee_name = if kind == "method_call_expression" {
            // Rust method calls: extract just the method name field
            node.child_by_field_name("name")
                .map(|n| node_text(&n, code).to_string())
        } else {
            // Regular calls: function/name/first-child
            node.child_by_field_name("function")
                .or_else(|| node.child_by_field_name("name"))
                .or_else(|| node.child(0))
                .map(|n| node_text(&n, code).to_string())
        };

        if let Some(name) = callee_name {
            // For qualified calls like "path::to::func", take just the last segment
            let short_name = name.rsplit("::").next().unwrap_or(&name);
            let short_name = short_name.rsplit('.').next().unwrap_or(short_name);
            if !short_name.is_empty() && short_name.len() < 200 {
                edges.push(Edge {
                    from_id: caller_id.to_string(),
                    to_id: format!("__unresolved::{short_name}"),
                    kind: "calls".to_string(),
                    ref_name: None,
                });
            }
        }
        return; // Don't recurse into call children
    }

    let mut child_cursor = node.walk();
    for child in node.children(&mut child_cursor) {
        extract_calls_recursive(&child, cursor, code, caller_id, edges);
    }
}

/// Resolve unresolved reference edges to actual node IDs.
///
/// The old predicate was `s.name == ref_name || s.qualified_name.ends_with("::"+
/// ref_name)`, scanning every node per edge (O(edges × nodes)). We precompute
/// two first-wins indexes so the common single-token ref resolves in O(1):
/// `by_name` (name → first node index) and `by_last_seg` (last `::` segment of
/// the qualified name → first node index). For a single-token ref, matching the
/// last segment is equivalent to the `ends_with("::"+ref)` suffix test. Picking
/// the smaller of the two indexes preserves the "first node in order" tie-break
/// of the original `find()`. Rare multi-segment refs fall back to a scan.
pub(crate) fn resolve_references(edges: &mut Vec<Edge>, nodes: &[Node]) {
    use std::collections::HashMap;
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    let mut by_last_seg: HashMap<&str, usize> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        by_name.entry(n.name.as_str()).or_insert(i);
        let seg = n
            .qualified_name
            .rsplit("::")
            .next()
            .unwrap_or(&n.qualified_name);
        by_last_seg.entry(seg).or_insert(i);
    }

    for edge in edges.iter_mut() {
        // Capture the raw reference token once (first extract) from the sentinel,
        // so resolution stays re-runnable over stored data even after to_id has
        // been rewritten to a concrete id.
        if edge.ref_name.is_none() {
            if let Some(name) = edge.to_id.strip_prefix("__unresolved::") {
                edge.ref_name = Some(name.to_string());
            }
        }
        // Concrete edges (ref_name == None: tests/overrides/exports) keep their
        // target id; name-reference edges resolve from the persisted raw name.
        let Some(ref_name) = edge.ref_name.clone() else {
            continue;
        };
        let idx = if ref_name.contains("::") {
            // Multi-segment ref: preserve exact suffix semantics via a scan.
            let needle = format!("::{ref_name}");
            by_name.get(ref_name.as_str()).copied().or_else(|| {
                nodes
                    .iter()
                    .position(|s| s.qualified_name.ends_with(&needle))
            })
        } else {
            match (
                by_name.get(ref_name.as_str()),
                by_last_seg.get(ref_name.as_str()),
            ) {
                (Some(&a), Some(&b)) => Some(a.min(b)),
                (Some(&a), None) | (None, Some(&a)) => Some(a),
                (None, None) => None,
            }
        };
        // Resolved -> real node id; unresolved -> keep the sentinel so the edge is
        // RETAINED (persisted with its ref_name) and re-resolves once the target
        // appears. Query paths filter sentinel endpoints, so this is query-neutral.
        edge.to_id = match idx {
            Some(i) => nodes[i].id.clone(),
            None => format!("__unresolved::{ref_name}"),
        };
    }
}

// ─── Markdown documentation extraction ───────────────────────────────

/// Extract documentation nodes from a Markdown file.
/// Creates a file node, section nodes (split on headings), and references edges
/// for backtick-quoted identifiers.
fn extract_markdown_doc(
    content: &str,
    rel_path: &str,
    source_name: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    // Create file node
    let file_qualified = format!("{source_name}::{rel_path}");
    let file_id = Node::id_for(source_name, &file_qualified);
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let file_hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let file_lines = content.lines().count();

    nodes.push(make_file_node(
        &file_id,
        file_name,
        &file_qualified,
        source_name,
        "markdown",
        rel_path,
        Some(&file_hash),
        file_lines,
    ));

    // Parse into sections by headings
    let mut current_heading: Option<String> = None;
    let mut current_body = String::new();
    let mut section_start_line = 1usize;

    for (i, line) in content.lines().enumerate() {
        if let Some(heading) = parse_md_heading(line) {
            // Flush previous section
            flush_doc_section(
                &current_heading,
                &current_body,
                section_start_line,
                rel_path,
                source_name,
                &file_id,
                nodes,
                edges,
            );
            current_heading = Some(heading);
            current_body.clear();
            section_start_line = i + 1;
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // Flush final section
    flush_doc_section(
        &current_heading,
        &current_body,
        section_start_line,
        rel_path,
        source_name,
        &file_id,
        nodes,
        edges,
    );
}

fn parse_md_heading(line: &str) -> Option<String> {
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
    Some(text)
}

fn flush_doc_section(
    heading: &Option<String>,
    body: &str,
    start_line: usize,
    file_path: &str,
    source_name: &str,
    file_id: &str,
    nodes: &mut Vec<Node>,
    edges: &mut Vec<Edge>,
) {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }

    let section_name = heading.as_deref().unwrap_or("(preamble)");
    let qualified = format!("{source_name}::{file_path}::{section_name}");
    let id = Node::id_for(source_name, &qualified);

    let body_text = format!("doc_section: {qualified}\n{section_name}\n{trimmed}");

    nodes.push(Node {
        id: id.clone(),
        kind: "doc_section".to_string(),
        name: section_name.to_string(),
        qualified_name: qualified,
        source_name: source_name.to_string(),
        language: "markdown".to_string(),
        file_path: file_path.to_string(),
        start_line,
        start_col: 0,
        end_line: 0,
        visibility: String::new(),
        signature: None,
        doc: Some(trimmed.to_string()),
        body: body_text,
        parent_id: Some(file_id.to_string()),
        content_hash: Some(blake3::hash(trimmed.as_bytes()).to_hex().to_string()),
        line_count: trimmed.lines().count(),
        source_url: None,
        description: None,
    });

    // Extract backtick references as edges to code symbols
    for cap in extract_backtick_refs(trimmed) {
        edges.push(Edge {
            from_id: id.clone(),
            to_id: format!("__unresolved::{cap}"),
            kind: "references".to_string(),
            ref_name: None,
        });
    }
}

/// Extract identifiers from backtick-quoted text in markdown.
fn extract_backtick_refs(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    let mut in_backtick = false;
    let mut current = String::new();

    for ch in text.chars() {
        if ch == '`' {
            if in_backtick {
                // End of backtick — check if it looks like an identifier
                let trimmed = current.trim();
                if !trimmed.is_empty()
                    && trimmed.len() < 100
                    && !trimmed.contains(' ')
                    // Skip things that look like code snippets, not identifiers
                    && !trimmed.contains('=')
                    && !trimmed.starts_with('-')
                    && !trimmed.starts_with('$')
                {
                    // Take the last component of a qualified name
                    let name = trimmed.rsplit([':', '.', '/']).next().unwrap_or(trimmed);
                    if !name.is_empty()
                        && name
                            .chars()
                            .next()
                            .map(|c| c.is_alphabetic())
                            .unwrap_or(false)
                    {
                        refs.push(name.to_string());
                    }
                }
                current.clear();
                in_backtick = false;
            } else {
                in_backtick = true;
            }
        } else if in_backtick {
            current.push(ch);
        }
    }

    refs.sort();
    refs.dedup();
    refs
}

/// Extract structured cross-reference targets from a doc-comment.
///
/// High precision by design: only author-MARKED references are recognized,
/// never bare prose tokens. This is the generalizable, cross-language
/// vocabulary bridge — the markup *is* the "this is code" signal, so there is
/// no English-word/symbol-name collision risk.
///
/// Recognized markup:
///   - rustdoc intra-doc links: `[Foo]`, `[`Foo`]`, `[a::b::C]`, `[txt](Foo)`
///   - Javadoc / JSDoc / KDoc:  `{@link Foo#bar}`, `{@linkplain Foo}`, `@see Foo`
///   - Sphinx / reST roles:     `:func:`mod.foo``, `:class:`Bar``
///   - C# XML doc:              `<see cref="T:Ns.Type.Member"/>`
///
/// Returns the last path component of each target (matching the
/// `resolve_references` name-resolution semantics), deduped.
fn extract_doc_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();

    // Javadoc / JSDoc inline tags: {@link TARGET ...}, {@linkplain TARGET ...}
    for tag in ["{@link", "{@linkplain"] {
        let mut from = 0;
        while let Some(p) = text[from..].find(tag) {
            let start = from + p + tag.len();
            // Boundary: `{@link` must not swallow `{@linkplain` (and vice versa);
            // the tag is followed by whitespace before the target.
            if !text[start..]
                .chars()
                .next()
                .map(|c| c.is_whitespace())
                .unwrap_or(false)
            {
                from = start;
                continue;
            }
            match text[start..].find('}') {
                Some(end_rel) => {
                    let inner = text[start..start + end_rel].trim();
                    // Target is the first token, ending at whitespace or '(' (params).
                    let target = inner.split([' ', '\t', '\n', '(']).next().unwrap_or("");
                    push_doc_ref(&mut out, target);
                    from = start + end_rel + 1;
                }
                None => break,
            }
        }
    }

    // C# XML doc: cref="TARGET" (also seealso, exception, etc.)
    {
        let mut from = 0;
        while let Some(p) = text[from..].find("cref=\"") {
            let start = from + p + 6;
            match text[start..].find('"') {
                Some(end_rel) => {
                    push_doc_ref(&mut out, &text[start..start + end_rel]);
                    from = start + end_rel + 1;
                }
                None => break,
            }
        }
    }

    for line in text.lines() {
        let trimmed = line.trim_start();
        // Javadoc / JSDoc block tag: @see TARGET (single token; skip URL/prose)
        if let Some(rest) = trimmed.strip_prefix("@see") {
            let rest = rest.trim();
            if let Some(tok) = rest.split([' ', '\t', '(']).next()
                && !tok.is_empty()
                && !tok.contains("://")
                && !tok.starts_with(['"', '<'])
            {
                push_doc_ref(&mut out, tok);
            }
        }
    }

    // Sphinx roles: a backtick span immediately preceded by `:` (`:func:`x``).
    // The closing `:` of the role sits right before the opening backtick — a
    // signal that does not occur in ordinary prose backticks.
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            // find closing backtick
            if let Some(close_rel) = text[i + 1..].find('`') {
                let inner = &text[i + 1..i + 1 + close_rel];
                if i > 0 && bytes[i - 1] == b':' {
                    push_doc_ref(&mut out, inner);
                }
                i = i + 1 + close_rel + 1;
                continue;
            }
        }
        i += 1;
    }

    // rustdoc intra-doc links: [TARGET] or [`TARGET`], and [text](TARGET).
    // Bracketed form only — distinguishes a link from a bare `code` span and
    // from normal markdown links/refs (followed by '(' URL, '[' ref, or ':' def).
    let mut i = 0;
    while i < bytes.len() {
        // Skip the label half of a reference-style link `[text][id]`: a '['
        // immediately preceded by ']' is a label, not an intra-doc link.
        if bytes[i] == b'[' && i > 0 && bytes[i - 1] == b']' {
            i += 1;
            continue;
        }
        if bytes[i] == b'['
            && let Some(close_rel) = text[i + 1..].find(']')
        {
            let inner = text[i + 1..i + 1 + close_rel].trim();
            let after = bytes.get(i + 1 + close_rel + 1).copied();
            match after {
                // [text](TARGET): take the parenthesized target if it's a
                // code path, not a URL.
                Some(b'(') => {
                    let tstart = i + 1 + close_rel + 2;
                    if let Some(tend_rel) = text[tstart..].find(')') {
                        let target = text[tstart..tstart + tend_rel].trim();
                        if !target.contains("://") && !target.starts_with(['#', '/', '.']) {
                            push_doc_ref(&mut out, target);
                        }
                    }
                }
                // [ref][id] reference-style link or [id]: def — not intra-doc.
                Some(b'[') | Some(b':') => {}
                // [Foo] / [`Foo`] shortcut intra-doc link.
                _ => push_doc_ref(&mut out, inner),
            }
            i = i + 1 + close_rel + 1;
            continue;
        }
        i += 1;
    }

    out.sort();
    out.dedup();
    out
}

/// Normalize a raw cross-reference target to a resolvable symbol name: strip
/// markup/decorations, drop any path qualifier, and keep only identifier-shaped
/// results. Pushes the last path component onto `out`.
fn push_doc_ref(out: &mut Vec<String>, raw: &str) {
    let raw = raw.trim().trim_matches('`').trim();
    // C# cref kind prefix: "T:Ns.Type" / "M:Ns.M" / "!:Unresolved" -> drop "X:".
    let raw = match raw.split_once(':') {
        Some((p, rest))
            if p.len() == 1 && p.chars().all(|c| c.is_ascii_uppercase() || c == '!') =>
        {
            rest
        }
        _ => raw,
    };
    // Sphinx "~" = render last component only; leading "." = relative ref.
    let raw = raw.trim_start_matches('~').trim_start_matches('.').trim();
    if raw.is_empty() || raw.len() > 100 || raw.contains(' ') || raw.contains("://") {
        return;
    }
    // Last component across path separators (Rust ::, Python/C# ., Java #, paths).
    let name = raw.rsplit([':', '.', '/', '#']).next().unwrap_or(raw);
    // Drop Java/JSDoc method parens: bar() -> bar.
    let name = name.split('(').next().unwrap_or(name);
    if name.is_empty() {
        return;
    }
    let first_ok = name
        .chars()
        .next()
        .map(|c| c.is_alphabetic() || c == '_')
        .unwrap_or(false);
    if first_ok && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        out.push(name.to_string());
    }
}

// ─── Language-specific node extraction ───────────────────────────────

fn node_text<'a>(node: &TsNode, code: &'a [u8]) -> &'a str {
    node.utf8_text(code).unwrap_or("")
}

fn find_child_by_kind<'a>(node: &TsNode<'a>, kind: &str) -> Option<TsNode<'a>> {
    let mut cursor = node.walk();
    node.children(&mut cursor).find(|c| c.kind() == kind)
}

fn extract_doc_comment(node: &TsNode, code: &[u8]) -> Option<String> {
    // Look for comment siblings immediately before this node. Attribute /
    // decorator nodes (e.g. Rust `#[derive(...)]`, Python `@dataclass`) sit
    // between the rustdoc and the declaration in the AST — treat them as
    // transparent so we keep walking until we either find a comment or hit
    // unrelated code.
    let mut comments = Vec::new();
    let mut sibling = node.prev_sibling();

    while let Some(sib) = sibling {
        match sib.kind() {
            "line_comment" | "comment" => {
                let text = node_text(&sib, code);
                let cleaned = text
                    .trim_start_matches("///")
                    .trim_start_matches("//!")
                    .trim_start_matches("//")
                    .trim_start_matches('#')
                    .trim();
                comments.push(cleaned.to_string());
                sibling = sib.prev_sibling();
            }
            "block_comment" | "doc_comment" => {
                let text = node_text(&sib, code);
                let cleaned = clean_block_comment(text);
                if !cleaned.is_empty() {
                    comments.push(cleaned);
                }
                sibling = sib.prev_sibling();
            }
            "attribute_item"
            | "inner_attribute_item"
            | "outer_attribute_item"
            | "attribute"
            | "decorator" => {
                sibling = sib.prev_sibling();
            }
            _ => break,
        }
    }

    if comments.is_empty() {
        return None;
    }
    comments.reverse();
    Some(comments.join("\n"))
}

fn extract_python_docstring(node: &TsNode, code: &[u8]) -> Option<String> {
    // Python docstrings are the first expression_statement in a function/class body
    let body = find_child_by_kind(node, "block")?;
    let mut cursor = body.walk();
    let first_stmt = body
        .children(&mut cursor)
        .find(|c| c.kind() == "expression_statement")?;
    let string_node = first_stmt.child(0)?;

    if string_node.kind() == "string" || string_node.kind() == "concatenated_string" {
        let text = node_text(&string_node, code);
        let cleaned = text
            .trim_start_matches("\"\"\"")
            .trim_end_matches("\"\"\"")
            .trim_start_matches("'''")
            .trim_end_matches("'''")
            .trim();
        if !cleaned.is_empty() {
            return Some(cleaned.to_string());
        }
    }
    None
}

fn clean_block_comment(text: &str) -> String {
    text.lines()
        .map(|line| {
            line.trim()
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches("* ")
                .trim_start_matches('*')
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// Language-specific extractors removed — tags.scm queries handle all supported languages.
// extract_generic_node below is the fallback for languages without a tags query.

/// Generic fallback extractor for unsupported languages.
/// Extracts common node types that appear across most tree-sitter grammars.
fn extract_generic_node(node: &TsNode, code: &[u8], kind: &str) -> Option<Node> {
    match kind {
        // Function-like declarations (covers C, C++, Java, Ruby, Shell, etc.)
        "function_definition"
        | "function_declaration"
        | "method_definition"
        | "method_declaration"
        | "function_item" // Rust-like
        | "subroutine"
        | "procedure" => {
            // C/C++: name is inside function_declarator child
            let declarator = find_child_by_kind(node, "function_declarator");
            let name_source = declarator.as_ref().unwrap_or(node);
            let name = find_child_by_kind(name_source, "identifier")
                .or_else(|| find_child_by_kind(name_source, "name"))
                .or_else(|| find_child_by_kind(name_source, "field_identifier"))
                .or_else(|| find_child_by_kind(name_source, "property_identifier"))
                .or_else(|| find_child_by_kind(node, "word"))?; // Bash
            let name = node_text(&name, code).to_string();
            if name.is_empty() || name.len() > 200 {
                return None;
            }
            let sig = extract_signature_text(node, code);
            let doc = extract_doc_comment(node, code);
            Some(make_node(&name, "function", sig, doc))
        }
        // Class/struct/type declarations
        "class_definition"
        | "class_declaration"
        | "class_specifier"     // C++
        | "struct_item"
        | "struct_declaration"
        | "struct_specifier"    // C++
        | "enum_item"
        | "enum_declaration"
        | "enum_specifier"      // C++
        | "interface_declaration"
        | "trait_item"
        | "type_declaration"
        | "record_declaration" => {
            let name = find_child_by_kind(node, "identifier")
                .or_else(|| find_child_by_kind(node, "type_identifier"))
                .or_else(|| find_child_by_kind(node, "name"))?;
            let name = node_text(&name, code).to_string();
            if name.is_empty() || name.len() > 200 {
                return None;
            }
            let doc = extract_doc_comment(node, code);
            let node_kind = if kind.contains("class") {
                "class"
            } else if kind.contains("struct") {
                "struct"
            } else if kind.contains("enum") {
                "enum"
            } else if kind.contains("interface") {
                "interface"
            } else if kind.contains("trait") {
                "trait"
            } else {
                "type"
            };
            Some(make_node(&name, node_kind, None, doc))
        }
        _ => None,
    }
}

fn detect_visibility(node: &TsNode, code: &[u8], lang: &str) -> String {
    match lang {
        "rust" => {
            if find_child_by_kind(node, "visibility_modifier").is_some() {
                "pub".to_string()
            } else {
                "private".to_string()
            }
        }
        "go" => {
            // Go exports uppercase names
            let name = find_child_by_kind(node, "identifier")
                .or_else(|| find_child_by_kind(node, "field_identifier"))
                .or_else(|| find_child_by_kind(node, "type_identifier"));
            if let Some(n) = name {
                let text = node_text(&n, code);
                if text.starts_with(|c: char| c.is_uppercase()) {
                    "pub".to_string()
                } else {
                    "private".to_string()
                }
            } else {
                String::new()
            }
        }
        "javascript" | "typescript" | "tsx" => {
            // Check if parent is an export_statement
            if let Some(parent) = node.parent()
                && parent.kind() == "export_statement"
            {
                return "export".to_string();
            }
            "private".to_string()
        }
        "python" => {
            if let Some(n) = find_child_by_kind(node, "identifier") {
                let text = node_text(&n, code);
                if text.starts_with('_') && !text.starts_with("__") {
                    "private".to_string()
                } else {
                    "pub".to_string()
                }
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn make_file_node(
    id: &str,
    name: &str,
    qualified_name: &str,
    source_name: &str,
    language: &str,
    file_path: &str,
    content_hash: Option<&str>,
    line_count: usize,
) -> Node {
    Node {
        id: id.to_string(),
        kind: "file".to_string(),
        name: name.to_string(),
        qualified_name: qualified_name.to_string(),
        source_name: source_name.to_string(),
        language: language.to_string(),
        file_path: file_path.to_string(),
        start_line: 0,
        start_col: 0,
        end_line: 0,
        visibility: String::new(),
        signature: None,
        doc: None,
        body: format!("file: {file_path}"),
        parent_id: None,
        content_hash: content_hash.map(|s| s.to_string()),
        line_count,
        source_url: None,
        description: None,
    }
}

fn make_node(name: &str, kind: &str, signature: Option<String>, doc: Option<String>) -> Node {
    let name = name.to_string();
    Node {
        id: String::new(),
        kind: kind.to_string(),
        name,
        qualified_name: String::new(),
        source_name: String::new(),
        language: String::new(),
        file_path: String::new(),
        start_line: 0,
        start_col: 0,
        end_line: 0,
        visibility: String::new(),
        signature,
        doc,
        body: String::new(),
        parent_id: None,
        content_hash: None,
        line_count: 0,
        source_url: None,
        description: None,
    }
}

/// Extract the signature line(s) from a node — everything up to the body.
fn extract_signature_text(node: &TsNode, code: &[u8]) -> Option<String> {
    let start = node.start_byte();
    // Find the body block (first { or : in the node)
    let body_node = find_child_by_kind(node, "block")
        .or_else(|| find_child_by_kind(node, "declaration_list"))
        .or_else(|| find_child_by_kind(node, "field_declaration_list"));

    let end = body_node
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte().min(start + 500));

    let sig = std::str::from_utf8(&code[start..end]).ok()?.trim();
    if sig.is_empty() {
        None
    } else {
        Some(sig.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detect_language_only_claims_parseable_languages() {
        // Every language detect_language returns must have a wired grammar,
        // otherwise the file is "detected" then silently dropped at parse time.
        for ext in [
            "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "go", "cpp", "cc", "hpp", "hh", "hxx",
            "h", "ino", "c", "sh", "bash",
        ] {
            let lang = detect_language(Path::new(&format!("f.{ext}")))
                .unwrap_or_else(|| panic!(".{ext} should be detected"));
            assert!(
                get_ts_language(lang).is_some(),
                ".{ext} -> {lang:?} has no tree-sitter grammar"
            );
        }
        // Extensions without a grammar must not be claimed.
        assert_eq!(detect_language(Path::new("Main.java")), None);
        assert_eq!(detect_language(Path::new("app.rb")), None);
    }

    #[test]
    fn test_extract_doc_refs_structured() {
        // rustdoc intra-doc links
        assert_eq!(extract_doc_refs("see [Foo] for details"), vec!["Foo"]);
        assert_eq!(extract_doc_refs("uses [`Bar`] internally"), vec!["Bar"]);
        assert_eq!(extract_doc_refs("path [a::b::Thing] link"), vec!["Thing"]);
        assert_eq!(
            extract_doc_refs("call [the method](Type::run) now"),
            vec!["run"]
        );
        // Javadoc / JSDoc / KDoc
        assert_eq!(extract_doc_refs("{@link Widget#render}"), vec!["render"]);
        assert_eq!(extract_doc_refs("{@linkplain Helper}"), vec!["Helper"]);
        assert_eq!(
            extract_doc_refs("@see SomeClass#doThing(int, int)"),
            vec!["doThing"]
        );
        // Sphinx / reST roles
        assert_eq!(extract_doc_refs(":func:`pkg.mod.compute`"), vec!["compute"]);
        assert_eq!(extract_doc_refs("see :class:`~pkg.Model`"), vec!["Model"]);
        // C# XML doc
        assert_eq!(
            extract_doc_refs("<see cref=\"T:Ns.Sub.Service\"/>"),
            vec!["Service"]
        );

        // Precision: bare prose and ordinary markdown links must NOT match.
        assert!(extract_doc_refs("just set the value and loop until done").is_empty());
        assert!(extract_doc_refs("a `code` span without a link").is_empty());
        assert!(extract_doc_refs("[docs](https://example.com/x)").is_empty());
        assert!(extract_doc_refs("ref style [text][id] and [id]: http://x").is_empty());
    }

    #[test]
    fn test_doc_refs_emit_reference_edges() {
        // A docstring on `dispatch` links to `handle`, defined elsewhere in the
        // file: the structured ref must become a resolved `references` edge.
        let g = extract_rust(
            "/// Routes the request; delegates to [`handle`].\n\
             pub fn dispatch() { let _ = 1; }\n\
             /// The real worker.\n\
             pub fn handle() { let _ = 2; }\n",
        );
        let dispatch = g
            .nodes
            .iter()
            .find(|n| n.name == "dispatch")
            .expect("dispatch node");
        let handle = g
            .nodes
            .iter()
            .find(|n| n.name == "handle")
            .expect("handle node");
        assert!(
            g.edges.iter().any(|e| e.from_id == dispatch.id
                && e.to_id == handle.id
                && e.kind == "references"),
            "expected resolved references edge dispatch -> handle"
        );
    }

    fn extract_rust(code: &str) -> FileGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_rust::LANGUAGE.into(),
            "rust",
            "test.rs",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph {
            nodes,
            edges,
            files: Vec::new(),
        }
    }

    fn extract_python(code: &str) -> FileGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_python::LANGUAGE.into(),
            "python",
            "test.py",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph {
            nodes,
            edges,
            files: Vec::new(),
        }
    }

    fn extract_js(code: &str) -> FileGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_javascript::LANGUAGE.into(),
            "javascript",
            "test.js",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph {
            nodes,
            edges,
            files: Vec::new(),
        }
    }

    fn extract_cpp(code: &str) -> FileGraph {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        extract_from_source(
            code,
            tree_sitter_cpp::LANGUAGE.into(),
            "cpp",
            "test.cpp",
            "test",
            "1.0.0",
            &mut nodes,
            &mut edges,
            None,
        )
        .unwrap();
        resolve_references(&mut edges, &nodes);
        FileGraph {
            nodes,
            edges,
            files: Vec::new(),
        }
    }

    #[test]
    fn test_rust_function() {
        let g = extract_rust("pub fn spawn() {}");
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].name, "spawn");
        assert_eq!(g.nodes[0].kind, "function");
    }

    #[test]
    fn test_rust_struct_and_impl() {
        let g = extract_rust(
            r#"
            pub struct Foo {}
            impl Foo {
                pub fn new() -> Self { Foo {} }
            }
            "#,
        );
        let names: Vec<&str> = g.nodes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"Foo"));
        assert!(names.contains(&"new"));
    }

    #[test]
    fn test_rust_trait() {
        let g = extract_rust(
            r#"
            pub trait Serializable {
                fn serialize(&self) -> String;
            }
            "#,
        );
        let kinds: Vec<&str> = g.nodes.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains(&"trait"));
    }

    #[test]
    fn test_rust_doc_comment() {
        let g = extract_rust(
            r#"
            /// Spawns a new task.
            pub fn spawn() {}
            "#,
        );
        assert_eq!(g.nodes.len(), 1);
        assert!(
            g.nodes[0]
                .doc
                .as_deref()
                .unwrap()
                .contains("Spawns a new task"),
            "doc: {:?}",
            g.nodes[0].doc
        );
    }

    #[test]
    fn test_rust_call_edges() {
        let g = extract_rust(
            r#"
            pub fn helper() {}
            pub fn main_fn() {
                helper();
            }
            "#,
        );
        let call_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "calls").collect();
        assert!(
            !call_edges.is_empty(),
            "should have call edges: {:?}",
            g.edges
        );
    }

    #[test]
    fn test_python_function() {
        let g = extract_python(
            r#"
def greet(name):
    """Say hello."""
    print(f"hello {name}")
            "#,
        );
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].name, "greet");
        assert!(g.nodes[0].doc.as_deref().unwrap().contains("Say hello"));
    }

    #[test]
    fn test_python_class() {
        let g = extract_python(
            r#"
class MyModel:
    """A model."""

    def predict(self, x):
        """Run prediction."""
        return x
            "#,
        );
        let names: Vec<&str> = g.nodes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"MyModel"));
        assert!(names.contains(&"predict"));
    }

    #[test]
    fn test_js_function() {
        let g = extract_js(
            r#"
            function greet(name) {
                return "hello " + name;
            }
            "#,
        );
        assert_eq!(g.nodes.len(), 1);
        assert_eq!(g.nodes[0].name, "greet");
    }

    #[test]
    fn test_js_class() {
        let g = extract_js(
            r#"
            class App {
                render() {
                    return null;
                }
            }
            "#,
        );
        let names: Vec<&str> = g.nodes.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"App"));
        assert!(names.contains(&"render"));
    }

    #[test]
    fn test_parent_child_relationship() {
        let g = extract_rust(
            r#"
            pub mod auth {
                pub fn login() {}
            }
            "#,
        );
        let login = g.nodes.iter().find(|s| s.name == "login").unwrap();
        assert!(
            login.parent_id.is_some(),
            "login should have a parent (the auth module)"
        );
    }

    #[test]
    fn test_qualified_names() {
        let g = extract_rust(
            r#"
            pub mod auth {
                pub fn login() {}
            }
            "#,
        );
        let login = g.nodes.iter().find(|s| s.name == "login").unwrap();
        assert!(
            login.qualified_name.contains("auth::login"),
            "got: {}",
            login.qualified_name
        );
    }

    #[test]
    fn test_extract_dir_with_file_nodes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn hello() {}\npub fn world() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        let file_nodes: Vec<_> = g.nodes.iter().filter(|n| n.kind == "file").collect();
        assert_eq!(file_nodes.len(), 1, "should have one file node");
        assert_eq!(file_nodes[0].name, "lib.rs");

        // Functions should have the file as parent
        let hello = g.nodes.iter().find(|n| n.name == "hello").unwrap();
        assert_eq!(hello.parent_id, Some(file_nodes[0].id.clone()));
    }

    #[test]
    fn test_visibility_detection() {
        let g = extract_rust(
            r#"
            pub fn public_fn() {}
            fn private_fn() {}
            "#,
        );
        let public = g.nodes.iter().find(|n| n.name == "public_fn").unwrap();
        assert_eq!(public.visibility, "pub");

        let private = g.nodes.iter().find(|n| n.name == "private_fn").unwrap();
        assert_eq!(private.visibility, "private");
    }

    #[test]
    fn test_rust_implements_edge() {
        let g = extract_rust(
            r#"
            pub trait Serialize {}
            pub struct Foo {}
            impl Serialize for Foo {}
            "#,
        );
        let impl_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "implements").collect();
        assert!(
            !impl_edges.is_empty(),
            "should have implements edge from impl Serialize for Foo"
        );
    }

    #[test]
    fn test_python_inherits_edge() {
        let g = extract_python(
            r#"
class Base:
    """Base class."""
    pass

class Child(Base):
    """Child class."""
    pass
            "#,
        );
        let inherits: Vec<_> = g.edges.iter().filter(|e| e.kind == "inherits").collect();
        assert!(
            !inherits.is_empty(),
            "should have inherits edge from Child to Base"
        );
    }

    #[test]
    fn test_cpp_function_with_calls() {
        // Pre-fix: tags captured `function_declarator` (declarator + params),
        // not `function_definition` (which contains the body), so the call
        // walker had no body to recurse into. After the fix, `inner` gets
        // body coverage and we should pick up the calls to `helper` and
        // `another`.
        let g = extract_cpp(
            r#"
int helper(int x) { return x + 1; }
int another(int y) { return y * 2; }
int outer(int z) {
    int a = helper(z);
    int b = another(z);
    return a + b;
}
            "#,
        );
        let funcs: Vec<_> = g.nodes.iter().filter(|n| n.kind == "function").collect();
        assert!(
            funcs.len() >= 3,
            "expected helper/another/outer, got {:?}",
            funcs.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
        let calls: Vec<_> = g.edges.iter().filter(|e| e.kind == "calls").collect();
        assert!(
            calls.len() >= 2,
            "expected at least 2 calls edges (outer→helper, outer→another), got {}: {:?}",
            calls.len(),
            calls,
        );
    }

    #[test]
    fn test_cpp_class_inherits() {
        let g = extract_cpp(
            r#"
class Base {
public:
    virtual void run();
};

class Derived : public Base {
public:
    void run() override;
};
            "#,
        );
        let inherits: Vec<_> = g.edges.iter().filter(|e| e.kind == "inherits").collect();
        assert!(
            !inherits.is_empty(),
            "expected inherits edge from Derived to Base, got edges {:?}",
            g.edges.iter().map(|e| &e.kind).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_cpp_method_call_and_type_ref() {
        let g = extract_cpp(
            r#"
struct Config {
    int value;
};

class Engine {
public:
    void start(Config cfg);
};

void Engine::start(Config cfg) {
    int v = cfg.value;
}
            "#,
        );
        let type_refs: Vec<_> = g.edges.iter().filter(|e| e.kind == "type_ref").collect();
        assert!(
            !type_refs.is_empty(),
            "expected at least one type_ref edge (Engine::start → Config), got {:?}",
            g.edges
                .iter()
                .map(|e| (&e.kind, &e.from_id, &e.to_id))
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_js_extends_edge() {
        let g = extract_js(
            r#"
            class Animal {}
            class Dog extends Animal {
                bark() {}
            }
            "#,
        );
        let inherits: Vec<_> = g.edges.iter().filter(|e| e.kind == "inherits").collect();
        assert!(
            !inherits.is_empty(),
            "should have inherits edge from Dog to Animal"
        );
    }

    #[test]
    fn test_python_visibility() {
        let g = extract_python(
            r#"
def public():
    """Public."""
    pass

def _private():
    """Private."""
    pass
            "#,
        );
        let names: Vec<&str> = g.nodes.iter().map(|n| n.name.as_str()).collect();
        // _private should be skipped by extractor (starts with _)
        assert!(names.contains(&"public"));
    }

    #[test]
    fn test_rust_type_ref_edges() {
        let g = extract_rust(
            r#"
            pub struct Config {}
            pub fn load(path: Config) -> Result {}
            "#,
        );
        let type_refs: Vec<_> = g.edges.iter().filter(|e| e.kind == "type_ref").collect();
        assert!(
            !type_refs.is_empty(),
            "should have type_ref edge for Config param: {:?}",
            g.edges
        );
    }

    #[test]
    fn test_markdown_doc_extraction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "# Getting Started\nInstall with `cargo`.\n\n## Usage\nCall `add` to ingest sources.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn add() {}\n").unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();

        // Should have doc_section nodes
        let doc_sections: Vec<_> = g.nodes.iter().filter(|n| n.kind == "doc_section").collect();
        assert!(
            doc_sections.len() >= 2,
            "should have at least 2 doc sections (Getting Started, Usage), got {}",
            doc_sections.len()
        );

        // Should have references edges from backtick mentions
        let refs: Vec<_> = g.edges.iter().filter(|e| e.kind == "references").collect();
        assert!(
            !refs.is_empty(),
            "should have references edges from backtick mentions"
        );
    }

    #[test]
    fn same_named_symbols_in_different_files_stay_distinct() {
        // Two top-level `helper`s in different files must not collide
        // on hash(source, qualified_name) and merge — with one's call edges
        // misattributed to the other.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "pub fn helper() { alpha(); }\npub fn alpha() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "pub fn helper() { beta(); }\npub fn beta() {}\n",
        )
        .unwrap();
        let g = extract_dir(dir.path(), "demo", "dev", Some("rust")).unwrap();

        let helpers: Vec<&Node> = g
            .nodes
            .iter()
            .filter(|n| n.name == "helper" && n.kind == "function")
            .collect();
        assert_eq!(
            helpers.len(),
            2,
            "two files each with a top-level helper() should yield two distinct nodes"
        );
        assert_ne!(helpers[0].id, helpers[1].id, "distinct ids");
        let files: std::collections::BTreeSet<&str> =
            helpers.iter().map(|h| h.file_path.as_str()).collect();
        assert_eq!(files.len(), 2, "the two helpers live in different files");

        // Edges stay correctly attributed: each helper calls the callee in ITS
        // file, not a merged blob pointing at both.
        let alpha = g.nodes.iter().find(|n| n.name == "alpha").unwrap();
        let beta = g.nodes.iter().find(|n| n.name == "beta").unwrap();
        let helper_a = helpers
            .iter()
            .find(|h| h.file_path.contains("a.rs"))
            .unwrap();
        let helper_b = helpers
            .iter()
            .find(|h| h.file_path.contains("b.rs"))
            .unwrap();
        assert!(
            g.edges
                .iter()
                .any(|e| e.from_id == helper_a.id && e.to_id == alpha.id),
            "helper in a.rs should call alpha"
        );
        assert!(
            g.edges
                .iter()
                .any(|e| e.from_id == helper_b.id && e.to_id == beta.id),
            "helper in b.rs should call beta"
        );
    }

    #[test]
    fn same_file_overloads_stay_distinct() {
        // C++ overloads share a signatureless qualified name; the
        // parameter list must keep them as separate nodes rather than collapsing
        // to one (with the extra signatures silently lost).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("calc.cpp"),
            "int add(int a, int b) { return a + b; }\n\
             double add(double a, double b) { return a + b; }\n\
             int add(int a, int b, int c) { return a + b + c; }\n",
        )
        .unwrap();
        let g = extract_dir(dir.path(), "demo", "dev", Some("cpp")).unwrap();

        let adds: Vec<&Node> = g.nodes.iter().filter(|n| n.name == "add").collect();
        assert_eq!(adds.len(), 3, "three overloads should yield three nodes");
        let ids: std::collections::BTreeSet<&str> = adds.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids.len(), 3, "each overload gets a distinct id");
    }

    #[test]
    fn prototype_and_definition_merge_keeping_the_definition() {
        // A forward declaration and its definition share a parameter
        // list, so they still merge — and the survivor must be the definition
        // (with the body) rather than the bare prototype.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("m.cpp"),
            "int compute(int x);\n\
             int compute(int x) {\n    return x * 2;\n}\n",
        )
        .unwrap();
        let g = extract_dir(dir.path(), "demo", "dev", Some("cpp")).unwrap();

        let computes: Vec<&Node> = g.nodes.iter().filter(|n| n.name == "compute").collect();
        assert_eq!(
            computes.len(),
            1,
            "prototype and definition merge to one node"
        );
        assert!(
            computes[0].end_line > computes[0].start_line,
            "survivor is the multi-line definition, not the one-line prototype"
        );
    }

    #[test]
    fn manifest_walk_matches_extraction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# Hi\nhello\n").unwrap();
        // Not indexable — no grammar for .txt, not markdown.
        std::fs::write(dir.path().join("notes.txt"), "ignore me\n").unwrap();
        // Skipped directory — must not appear in either walk.
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/junk.rs"), "pub fn z() {}\n").unwrap();

        let manifest = extract_dir(dir.path(), "demo", "dev", None).unwrap().files;
        let listed = list_source_files(dir.path(), None).unwrap();

        let as_set = |v: &[FileMeta]| {
            v.iter()
                .map(|f| (f.path.clone(), f.content_hash.clone()))
                .collect::<std::collections::BTreeSet<_>>()
        };
        // The cheap no-parse walk must agree with extraction on path + hash.
        assert_eq!(
            as_set(&manifest),
            as_set(&listed),
            "list_source_files must match extract_dir's manifest"
        );
        let paths: std::collections::BTreeSet<&str> =
            listed.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains("src/lib.rs") && paths.contains("README.md"));
        assert!(
            !paths
                .iter()
                .any(|p| p.contains("notes.txt") || p.contains("target"))
        );
    }

    #[test]
    fn test_backtick_ref_extraction() {
        let refs = extract_backtick_refs("Use `spawn` to create tasks. See `tokio::Runtime`.");
        assert!(refs.contains(&"spawn".to_string()));
        assert!(refs.contains(&"Runtime".to_string()));
    }

    #[test]
    fn test_python_decorator_edge() {
        let g = extract_python(
            r#"
def cache(fn):
    """Cache decorator."""
    pass

@cache
def expensive():
    """Expensive computation."""
    pass
            "#,
        );
        let decorates: Vec<_> = g.edges.iter().filter(|e| e.kind == "decorates").collect();
        assert!(
            !decorates.is_empty(),
            "should have decorates edge: {:?}",
            g.edges
        );
    }

    #[test]
    fn test_test_edges_inferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn spawn() {}\npub fn test_spawn() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        let test_edges: Vec<_> = g.edges.iter().filter(|e| e.kind == "tests").collect();
        assert!(
            !test_edges.is_empty(),
            "should infer test_spawn tests spawn"
        );
    }

    #[test]
    fn test_export_edges_inferred() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn public_api() {}\nfn private_impl() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "mylib", "1.0.0", Some("rust")).unwrap();
        let exports: Vec<_> = g.edges.iter().filter(|e| e.kind == "exports").collect();
        assert!(
            !exports.is_empty(),
            "pub functions should get exports edges from file"
        );
    }

    #[test]
    fn test_tests_edge_case_insensitive_and_camelcase_matching() {
        // Pins `extract_tested_name` + the ASCII case-insensitive candidate
        // match: `test_widget` strips to "widget" (lowercase) but must still
        // resolve to the differently-cased `Widget` symbol; `testWidget`
        // (camelCase JS-style convention) strips to "Widget" (exact case);
        // `TestFoo` (PascalCase convention) strips to "Foo" (exact case).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn Widget() {}\n\
             pub fn test_widget() {}\n\
             pub fn testWidget() {}\n\
             pub fn Foo() {}\n\
             pub fn TestFoo() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "inf", "0", Some("rust")).unwrap();
        let id_to_name: std::collections::HashMap<&str, &str> = g
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.name.as_str()))
            .collect();

        let has_tests_edge = |from: &str, to: &str| -> bool {
            g.edges.iter().any(|e| {
                e.kind == "tests"
                    && id_to_name.get(e.from_id.as_str()).copied() == Some(from)
                    && id_to_name.get(e.to_id.as_str()).copied() == Some(to)
            })
        };

        assert!(
            has_tests_edge("test_widget", "Widget"),
            "test_widget should test Widget despite the case difference (widget vs Widget)"
        );
        assert!(
            has_tests_edge("testWidget", "Widget"),
            "camelCase testWidget should test Widget"
        );
        assert!(
            has_tests_edge("TestFoo", "Foo"),
            "PascalCase TestFoo should test Foo"
        );
    }

    #[test]
    fn test_tests_edge_first_match_for_ambiguous_candidate_name() {
        // Two files each define a symbol named `helper`; a single test targets
        // that name. The inferred edge must land on the candidate that sorts
        // FIRST in canonical (file_path, start_line, id) order — a_mod.rs —
        // not b_mod.rs, pinning first-match determinism through the hashed
        // lookup refactor.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a_mod.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(dir.path().join("b_mod.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(dir.path().join("caller.rs"), "pub fn test_helper() {}\n").unwrap();

        let g = extract_dir(dir.path(), "inf", "0", Some("rust")).unwrap();

        let test_helper = g
            .nodes
            .iter()
            .find(|n| n.name == "test_helper")
            .expect("test_helper node");
        let tests_edge = g
            .edges
            .iter()
            .find(|e| e.kind == "tests" && e.from_id == test_helper.id)
            .expect("test_helper should get a tests edge to one of the `helper` candidates");

        let target = g
            .nodes
            .iter()
            .find(|n| n.id == tests_edge.to_id)
            .expect("edge target node exists in the graph");
        assert_eq!(target.name, "helper");
        assert_eq!(
            target.file_path, "a_mod.rs",
            "ambiguous `helper` should resolve to the canonically-first candidate \
             (a_mod.rs sorts before b_mod.rs); resolved to {}",
            target.file_path
        );
    }

    #[test]
    fn test_overrides_edge_inferred_from_python_inheritance() {
        // Derived(Base) both define `handle`; the override pass must link
        // Derived.handle -> Base.handle across the (finalize-resolved)
        // `inherits` edge.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("shapes.py"),
            "class Base:\n    def handle(self):\n        return 1\n\n\nclass Derived(Base):\n    def handle(self):\n        return 2\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "inf", "0", Some("python")).unwrap();

        let base = g
            .nodes
            .iter()
            .find(|n| n.name == "Base" && n.kind == "class")
            .expect("Base class node");
        let derived = g
            .nodes
            .iter()
            .find(|n| n.name == "Derived" && n.kind == "class")
            .expect("Derived class node");

        // The override assertion below is only meaningful if the fixture
        // genuinely produced an inherits edge — verify that first so a
        // regression in `inherits` extraction can't leave this vacuously true.
        assert!(
            g.edges
                .iter()
                .any(|e| e.kind == "inherits" && e.from_id == derived.id && e.to_id == base.id),
            "fixture must produce an inherits edge Derived -> Base; got edges {:?}",
            g.edges
                .iter()
                .map(|e| (&e.kind, &e.from_id, &e.to_id))
                .collect::<Vec<_>>()
        );

        let base_handle = g
            .nodes
            .iter()
            .find(|n| n.name == "handle" && n.parent_id.as_deref() == Some(base.id.as_str()))
            .expect("Base.handle method node");
        let derived_handle = g
            .nodes
            .iter()
            .find(|n| n.name == "handle" && n.parent_id.as_deref() == Some(derived.id.as_str()))
            .expect("Derived.handle method node");

        assert!(
            g.edges.iter().any(|e| e.kind == "overrides"
                && e.from_id == derived_handle.id
                && e.to_id == base_handle.id),
            "expected overrides edge from Derived.handle to Base.handle"
        );
    }

    #[test]
    fn test_tests_edge_not_inferred_when_no_matching_symbol() {
        // Guards against spurious matches: a test symbol whose conventional
        // tested name has no corresponding non-test candidate must not get a
        // `tests` edge to anything.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn test_orphan() {}\npub fn unrelated() {}\n",
        )
        .unwrap();

        let g = extract_dir(dir.path(), "inf", "0", Some("rust")).unwrap();
        let test_orphan = g
            .nodes
            .iter()
            .find(|n| n.name == "test_orphan")
            .expect("test_orphan node");
        let has_tests_edge = g
            .edges
            .iter()
            .any(|e| e.kind == "tests" && e.from_id == test_orphan.id);
        assert!(
            !has_tests_edge,
            "test_orphan has no matching `orphan` symbol; must not get a tests edge"
        );
    }

    #[test]
    fn test_backtick_skips_non_identifiers() {
        let refs = extract_backtick_refs("Run `cargo build --release` and `export PATH=$HOME`.");
        // These contain spaces, =, or $ — should be skipped
        assert!(refs.is_empty(), "should skip non-identifiers: {:?}", refs);
    }

    /// Sorts a graph's nodes by `id` and its edges by `(from_id, to_id, kind,
    /// ref_name)` so two independently-produced graphs can be compared for
    /// exact equality regardless of internal ordering. Both are total orders
    /// after `finalize_graph` (unique ids; deduped `(from_id, to_id, kind)`),
    /// so this comparison key is stable and reproducible.
    fn sorted_graph(g: &FileGraph) -> (Vec<Node>, Vec<Edge>) {
        let mut nodes = g.nodes.clone();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        let mut edges = g.edges.clone();
        edges.sort_by(|a, b| {
            (&a.from_id, &a.to_id, &a.kind, &a.ref_name).cmp(&(
                &b.from_id,
                &b.to_id,
                &b.kind,
                &b.ref_name,
            ))
        });
        (nodes, edges)
    }

    #[test]
    fn reextract_incremental_matches_full_after_broken_cross_file_reference() {
        // A (unchanged) calls target(), defined in B. Renaming target in B
        // (A untouched) must leave A's call unresolved in both a full
        // re-extraction and an incremental one.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/b")).unwrap();
        std::fs::write(
            dir.path().join("src/a/mod.rs"),
            "pub fn caller() { target(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/b/mod.rs"), "pub fn target() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# Demo\n").unwrap();

        let prior = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        std::fs::write(
            dir.path().join("src/b/mod.rs"),
            "pub fn target_renamed() {}\n",
        )
        .unwrap();

        let incremental =
            reextract_incremental(dir.path(), "eq", "0", Some("rust"), &prior).unwrap();
        let full = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        assert_eq!(sorted_graph(&incremental), sorted_graph(&full));

        // Guard against a vacuous pass: the renamed-away call must actually be
        // the unresolved sentinel, not silently dropped or stale.
        assert!(
            incremental
                .edges
                .iter()
                .any(|e| e.to_id == "__unresolved::target"),
            "expected caller's reference to the renamed target to be unresolved: {:?}",
            incremental.edges
        );
    }

    #[test]
    fn reextract_incremental_resolves_kept_edge_to_newly_added_symbol() {
        // A (unchanged) calls future_fn(), which B does not define yet — the
        // call stays as a retained-but-unresolved edge in `prior`. Adding
        // future_fn to B must re-resolve A's kept edge to the new concrete
        // node, in both a full and an incremental re-extraction.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/caller")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/callee")).unwrap();
        std::fs::write(
            dir.path().join("src/caller/mod.rs"),
            "pub fn invoke() { future_fn(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/callee/mod.rs"),
            "pub fn placeholder() {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Demo\n").unwrap();

        let prior = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();
        let invoke = prior
            .nodes
            .iter()
            .find(|n| n.name == "invoke")
            .expect("invoke node")
            .clone();
        assert!(
            prior
                .edges
                .iter()
                .any(|e| e.from_id == invoke.id && e.to_id == "__unresolved::future_fn"),
            "prior should retain an unresolved future_fn call: {:?}",
            prior.edges
        );

        std::fs::write(
            dir.path().join("src/callee/mod.rs"),
            "pub fn placeholder() {}\npub fn future_fn() {}\n",
        )
        .unwrap();

        let incremental =
            reextract_incremental(dir.path(), "eq", "0", Some("rust"), &prior).unwrap();
        let full = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        assert_eq!(sorted_graph(&incremental), sorted_graph(&full));

        let future_fn = incremental
            .nodes
            .iter()
            .find(|n| n.name == "future_fn")
            .expect("future_fn node from the edited file");
        assert!(
            incremental
                .edges
                .iter()
                .any(|e| e.from_id == invoke.id && e.to_id == future_fn.id && e.kind == "calls"),
            "expected invoke -> future_fn to resolve to the concrete node id: {:?}",
            incremental.edges
        );
    }

    #[test]
    fn reextract_incremental_matches_full_order_for_ambiguous_symbol_names() {
        // Two files each define `shared`; a third unchanged file calls it.
        // Editing the alphabetically-first `shared` definer must not change
        // which node the ambiguous reference resolves to relative to a full
        // re-extraction — incremental must reconstruct nodes in the same
        // (manifest) order a full walk would.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/aaa_changed")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/bbb_unchanged")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/ccc_caller")).unwrap();
        std::fs::write(
            dir.path().join("src/aaa_changed/mod.rs"),
            "pub fn shared() { let _flag = 1; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/bbb_unchanged/mod.rs"),
            "pub fn shared() { let _flag = 2; }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/ccc_caller/mod.rs"),
            "pub fn invoke() { shared(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "# Demo\n").unwrap();

        let prior = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        std::fs::write(
            dir.path().join("src/aaa_changed/mod.rs"),
            "pub fn shared() { let _flag = 3; }\n",
        )
        .unwrap();

        let incremental =
            reextract_incremental(dir.path(), "eq", "0", Some("rust"), &prior).unwrap();
        let full = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        assert_eq!(sorted_graph(&incremental), sorted_graph(&full));
    }

    #[test]
    fn reextract_incremental_matches_full_after_markdown_edit() {
        // Editing README.md (heading + a backticked identifier) must be
        // handled by the incremental path the same as a full re-extraction,
        // proving markdown files re-extract correctly, not just code files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/b")).unwrap();
        std::fs::write(dir.path().join("src/a/mod.rs"), "pub fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("src/b/mod.rs"), "pub fn beta() {}\n").unwrap();
        std::fs::write(
            dir.path().join("README.md"),
            "# Getting Started\nCall `alpha` to begin.\n",
        )
        .unwrap();

        let prior = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        std::fs::write(
            dir.path().join("README.md"),
            "# Usage Guide\nCall `beta` now, not `alpha`.\n",
        )
        .unwrap();

        let incremental =
            reextract_incremental(dir.path(), "eq", "0", Some("rust"), &prior).unwrap();
        let full = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        assert_eq!(sorted_graph(&incremental), sorted_graph(&full));

        // Guard against a vacuous pass: the edited heading must actually show
        // up as a doc_section, proving the markdown was really re-parsed.
        assert!(
            incremental
                .nodes
                .iter()
                .any(|n| n.kind == "doc_section" && n.name == "Usage Guide"),
            "expected the edited README heading in the incremental graph: {:?}",
            incremental
                .nodes
                .iter()
                .map(|n| &n.name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reextract_incremental_matches_full_after_add_and_delete_file() {
        // Adding src/c.rs (defining a symbol an existing file references) and
        // deleting an unrelated existing file must both be reflected
        // identically by the incremental path and a full re-extraction.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/keep_a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/keep_b")).unwrap();
        std::fs::write(
            dir.path().join("src/keep_a/mod.rs"),
            "pub fn wants_gadget() { gadget(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/keep_b/mod.rs"), "pub fn doomed() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# Demo\n").unwrap();

        let prior = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();
        assert!(
            prior
                .edges
                .iter()
                .any(|e| e.to_id == "__unresolved::gadget"),
            "prior should retain an unresolved gadget call: {:?}",
            prior.edges
        );

        std::fs::write(dir.path().join("src/c.rs"), "pub fn gadget() {}\n").unwrap();
        std::fs::remove_file(dir.path().join("src/keep_b/mod.rs")).unwrap();
        std::fs::remove_dir(dir.path().join("src/keep_b")).unwrap();

        let incremental =
            reextract_incremental(dir.path(), "eq", "0", Some("rust"), &prior).unwrap();
        let full = extract_dir(dir.path(), "eq", "0", Some("rust")).unwrap();

        assert_eq!(sorted_graph(&incremental), sorted_graph(&full));

        // Guard against a vacuous pass: the deleted node must be gone and the
        // added node must be present and resolved to, not just coincidentally
        // equal empty sets.
        assert!(
            !incremental.nodes.iter().any(|n| n.name == "doomed"),
            "deleted file's node must not survive incremental re-extraction"
        );
        let gadget = incremental
            .nodes
            .iter()
            .find(|n| n.name == "gadget")
            .expect("gadget node from the added file");
        assert!(
            incremental
                .edges
                .iter()
                .any(|e| e.to_id == gadget.id && e.kind == "calls"),
            "expected wants_gadget -> gadget to resolve to the concrete added node"
        );
    }

    /// Build a minimal valid `Node` for finalize_graph tests.
    fn tnode(name: &str, kind: &str, qualified: &str) -> Node {
        Node {
            id: Node::id_for("test", qualified),
            kind: kind.to_string(),
            name: name.to_string(),
            qualified_name: qualified.to_string(),
            source_name: "test".to_string(),
            language: "rust".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            visibility: "pub".to_string(),
            signature: Some(format!("fn {name}()")),
            doc: None,
            body: format!("function: {qualified}"),
            parent_id: None,
            content_hash: None,
            line_count: 10,
            source_url: None,
            description: None,
        }
    }

    #[test]
    fn finalize_graph_drops_edges_with_empty_from_id() {
        let caller = tnode("caller", "function", "lib::caller");
        let callee = tnode("callee", "function", "lib::callee");
        let caller_id = caller.id.clone();
        let callee_id = callee.id.clone();
        let mut nodes = vec![caller, callee];
        let mut edges = vec![
            Edge {
                from_id: caller_id.clone(),
                to_id: callee_id.clone(),
                kind: "calls".to_string(),
                ref_name: None,
            },
            // No traversable source node: must be dropped, or the empty
            // string id would seed graph traversal.
            Edge {
                from_id: String::new(),
                to_id: callee_id.clone(),
                kind: "calls".to_string(),
                ref_name: None,
            },
        ];

        finalize_graph(&mut nodes, &mut edges);

        assert!(
            edges.iter().all(|e| !e.from_id.is_empty()),
            "no surviving edge should have an empty from_id: {edges:?}"
        );
        assert!(
            edges
                .iter()
                .any(|e| e.from_id == caller_id && e.kind == "calls"),
            "the valid caller -> callee edge must survive: {edges:?}"
        );
    }
}
