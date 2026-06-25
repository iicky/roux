use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::{Config, StoreScope};
use crate::graph;
use crate::graph::extract::FileGraph;
use crate::graph::store::GraphStore;
use crate::source::Source;
use crate::source::SourceKind;

const DEFAULT_CRATE_TIMEOUT_SECS: u64 = 30;

#[derive(Parser)]
#[command(name = "roux", about = "Prep fresh docs for your agents")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Detect project type and ingest docs for all dependencies
    Init {
        /// Include transitive dependencies
        #[arg(long)]
        transitive: bool,
        /// Write to .roux/db.sqlite instead of global
        #[arg(long)]
        local: bool,
        /// Skip dependencies matching a glob pattern (repeatable, e.g. --exclude 'candle*')
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Per-crate extraction timeout in seconds
        #[arg(long, default_value_t = DEFAULT_CRATE_TIMEOUT_SECS)]
        timeout: u64,
    },
    /// Ingest a source into the index
    Add {
        /// Source: crate name, local path, URL, or OpenAPI spec
        source: String,
        /// Override language detection
        #[arg(long)]
        lang: Option<String>,
        /// Write to .roux/db.sqlite instead of global
        #[arg(long)]
        local: bool,
        /// Pin a specific version
        #[arg(long)]
        version: Option<String>,
        /// Override display name for the source
        #[arg(long)]
        name: Option<String>,
    },
    /// Retrieve relevant chunks for a query
    Query {
        /// Query string
        query: String,
        /// Extra query variant(s), fused with the main query via reciprocal-rank
        /// fusion. Repeatable: reformulate one question into the code's jargon.
        #[arg(long = "also", value_name = "QUERY")]
        also: Vec<String>,
        /// Number of results
        #[arg(long, default_value = "3")]
        top: usize,
        /// Restrict search to a named source
        #[arg(long)]
        source: Option<String>,
        /// Output format: text, json, or skeleton (compact prompt-prefix block)
        #[arg(long, default_value = "text")]
        format: String,
        /// Search local index only (mutually exclusive with --global)
        #[arg(long, conflicts_with = "global")]
        local: bool,
        /// Search global index only (mutually exclusive with --local)
        #[arg(long)]
        global: bool,
        /// Query a specific .sqlite index file (e.g. a downloaded artifact)
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
    },
    /// List all indexed sources
    List {
        /// Output format: text or json
        #[arg(long, default_value = "text")]
        format: String,
        /// List local index only (mutually exclusive with --global)
        #[arg(long, conflicts_with = "global")]
        local: bool,
        /// List global index only (mutually exclusive with --local)
        #[arg(long)]
        global: bool,
        /// List sources in a specific .sqlite index file
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
    },
    /// Re-read lockfile and re-ingest changed dependencies
    Sync {
        /// Sync a specific source
        source: Option<String>,
        /// Show what would be re-ingested without doing it
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a source and all its chunks
    Remove {
        /// Source name to remove
        source: String,
    },
    /// Run as an MCP server over stdio for AI agent integration
    Serve {
        /// Serve the local index only (mutually exclusive with --global)
        #[arg(long, conflicts_with = "global")]
        local: bool,
        /// Serve the global index only (mutually exclusive with --local)
        #[arg(long)]
        global: bool,
        /// Serve a specific .sqlite index file (e.g. a downloaded artifact)
        #[arg(long, value_name = "PATH")]
        db: Option<std::path::PathBuf>,
    },
    /// Export the local index to a portable artifact file for distribution
    Export {
        /// Output path (e.g. my-index.sqlite or my-index.sqlite.gz)
        #[arg(long, value_name = "PATH")]
        output: std::path::PathBuf,
        /// Compress the output with gzip (appends/requires .gz suffix)
        #[arg(long)]
        gzip: bool,
        /// Export from the global index instead of .roux/db.sqlite
        #[arg(long)]
        global: bool,
    },
}

impl Cli {
    /// Parse from an iterator of arguments (for testing).
    pub fn try_parse_from<I, T>(iter: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        <Self as Parser>::try_parse_from(iter)
    }

    pub fn run(&self) -> Result<()> {
        let config = Config::load()?;

        match &self.command {
            Command::Init {
                transitive,
                local,
                exclude,
                timeout,
            } => cmd_init(&config, *transitive, *local, exclude, *timeout),
            Command::Add {
                source,
                lang,
                local,
                version,
                name,
            } => cmd_add(
                &config,
                source,
                name.clone(),
                lang.clone(),
                version.clone(),
                *local,
            ),
            Command::Query {
                query,
                also,
                top,
                source,
                format,
                local,
                global,
                db,
            } => cmd_query(
                &config,
                query,
                also,
                *top,
                source.as_deref(),
                format,
                *local,
                *global,
                db.as_deref(),
            ),
            Command::List {
                format,
                local,
                global,
                db,
            } => cmd_list(&config, format, *local, *global, db.as_deref()),
            Command::Serve { local, global, db } => {
                cmd_serve(&config, *local, *global, db.as_deref())
            }
            Command::Sync { source, dry_run } => cmd_sync(&config, source.as_deref(), *dry_run),
            Command::Remove { source } => cmd_remove(&config, source),
            Command::Export {
                output,
                gzip,
                global,
            } => cmd_export(&config, output, *gzip, *global),
        }
    }
}

/// JSON shape for a single `SearchResult` — matched IDs, ranked symbols (with
/// graph neighborhood), and cross-edges. Shared by `roux query --format json`
/// and the MCP `roux_query` tool.
pub fn search_result_to_json(result: &crate::graph::store::SearchResult) -> serde_json::Value {
    serde_json::json!({
        "matched": result.matched_ids,
        "symbols": result.nodes.iter().map(|s| {
            serde_json::json!({
                "id": s.id,
                "kind": s.kind,
                "name": s.name,
                "qualified_name": s.qualified_name,
                "file": s.file_path,
                "line": s.start_line,
                "signature": s.signature,
                "doc": s.doc,
                "parent_id": s.parent_id,
                "matched": result.matched_ids.contains(&s.id),
                "score": result.scores.get(&s.id),
            })
        }).collect::<Vec<_>>(),
        "edges": result.edges.iter().map(|e| {
            serde_json::json!({
                "from": e.from_id,
                "to": e.to_id,
                "kind": e.kind,
            })
        }).collect::<Vec<_>>(),
    })
}

/// JSON shape for `roux list`. Shared by the CLI handler and the MCP
/// `roux_list` tool.
pub fn list_rows_to_json(
    rows: &[(crate::graph::store::SourceRecord, Status, String)],
) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|(s, status, display)| {
            serde_json::json!({
                "name": display,
                "version": s.version,
                "language": s.language,
                "symbols": s.node_count,
                "ingested_at": s.ingested_at,
                "source_kind": s.source_kind,
                "origin": s.origin,
                "status": status.label(),
                "stale_reason": status.reason(),
            })
        })
        .collect();
    serde_json::Value::Array(arr)
}

fn cmd_init(
    config: &Config,
    transitive: bool,
    local: bool,
    exclude: &[String],
    timeout_secs: u64,
) -> Result<()> {
    let cwd = std::env::current_dir()?;

    let project = crate::lockfile::detect_project(&cwd)
        .ok_or_else(|| anyhow::anyhow!("No lockfile or manifest found in current directory"))?;

    let direct_count = project.deps.iter().filter(|d| d.direct).count();
    let total_count = project.deps.len();
    eprintln!(
        "Detected {:?} project ({} direct deps, {} total) from {}",
        project.kind,
        direct_count,
        total_count,
        project
            .lockfile
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
    );

    let deps: Vec<_> = if transitive {
        project.deps
    } else {
        project.deps.into_iter().filter(|d| d.direct).collect()
    };

    if deps.is_empty() {
        eprintln!("No dependencies to ingest.");
        return Ok(());
    }

    eprintln!("Ingesting {} dependencies...\n", deps.len());

    let store_path = config.resolve_store_path(StoreScope::from_flags(local, false));
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = GraphStore::open(&store_path)?;

    let timeout = Duration::from_secs(timeout_secs);

    let mut success = 0;
    let mut excluded = 0;
    let mut skipped = 0;
    let mut failed = 0;

    for dep in &deps {
        let version_str = dep.version.as_deref().unwrap_or("latest");

        if exclude.iter().any(|p| matches_glob(p, &dep.name)) {
            eprintln!("  {} (excluded)", dep.name);
            excluded += 1;
            continue;
        }

        // For Rust crates, download from crates.io
        if project.kind == crate::lockfile::ProjectKind::Rust {
            eprint!("  {} v{} ... ", dep.name, version_str);

            match extract_crate_with_timeout(&dep.name, version_str, timeout) {
                CrateOutcome::Ok { version, graph } => {
                    let count = graph.nodes.len();
                    if count == 0 {
                        eprintln!("0 symbols");
                        success += 1;
                        continue;
                    }
                    let upsert = store
                        .upsert_source(&dep.name, &version, "rust", &graph.nodes, &graph.edges)
                        .and_then(|()| {
                            store.set_source_meta(
                                &dep.name,
                                "crate",
                                Some(&dep.name),
                                Some(&version),
                            )
                        });
                    match upsert {
                        Ok(()) => {
                            eprintln!("{count} symbols");
                            success += 1;
                        }
                        Err(e) => {
                            eprintln!("failed: {e}");
                            failed += 1;
                        }
                    }
                }
                CrateOutcome::Err(e) => {
                    eprintln!("failed: {e}");
                    failed += 1;
                }
                CrateOutcome::Timeout => {
                    eprintln!("timeout after {timeout_secs}s");
                    failed += 1;
                }
            }
        } else {
            // For other languages, we can only ingest local paths
            // TODO: add PyPI, npm registry support
            eprintln!(
                "  {} (skip — no registry support for {:?} yet)",
                dep.name, project.kind
            );
            skipped += 1;
        }
    }

    eprintln!(
        "\nDeps: {success} ingested, {excluded} excluded, {skipped} skipped, {failed} failed"
    );

    // Index the local source
    let project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    eprintln!("\nIndexing local source as '{project_name}'...");
    let file_graph =
        graph::extract::extract_dir(&cwd, project_name, "dev", Some(project.kind.language()))?;
    if !file_graph.nodes.is_empty() {
        store.upsert_source(
            project_name,
            "dev",
            project.kind.language(),
            &file_graph.nodes,
            &file_graph.edges,
        )?;
        let fp = crate::fingerprint::fingerprint_dir(&cwd).ok();
        store.set_source_meta(project_name, "path", cwd.to_str(), fp.as_deref())?;
        eprintln!(
            "Indexed {} symbols, {} edges from local source",
            file_graph.nodes.len(),
            file_graph.edges.len()
        );
    }

    // Store lockfile hash for staleness detection
    if let Ok(content) = std::fs::read(&project.lockfile) {
        let hash = blake3::hash(&content).to_hex().to_string();
        store.set_metadata("lockfile_hash", &hash)?;
        store.set_metadata("lockfile_path", &project.lockfile.to_string_lossy())?;
    }

    Ok(())
}

enum CrateOutcome {
    Ok { version: String, graph: FileGraph },
    Err(anyhow::Error),
    Timeout,
}

/// Download and extract a crate on a worker thread, bailing if it runs past
/// `timeout`. On timeout the worker is left running — the process exits soon
/// after init completes, and nothing SQLite-related happens in the worker so
/// a leaked thread won't hold a write lock on the store.
fn extract_crate_with_timeout(name: &str, version: &str, timeout: Duration) -> CrateOutcome {
    let name = name.to_owned();
    let version = version.to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| -> Result<(String, FileGraph)> {
            let (dir, resolved) = crate::source::crate_download::download_crate(&name, &version)?;
            let graph = graph::extract::extract_dir(&dir, &name, &resolved, Some("rust"))?;
            Ok((resolved, graph))
        })();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok((version, graph))) => CrateOutcome::Ok { version, graph },
        Ok(Err(e)) => CrateOutcome::Err(e),
        Err(_) => CrateOutcome::Timeout,
    }
}

/// Case-insensitive glob match with `*` as "zero or more characters".
/// Supports patterns like `candle`, `candle*`, `*candle`, `*candle*`, `foo*bar`.
fn matches_glob(pattern: &str, name: &str) -> bool {
    let pattern = pattern.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let last = parts.len() - 1;
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !name[pos..].starts_with(part) {
                return false;
            }
            pos += part.len();
        } else if i == last {
            return name[pos..].ends_with(part);
        } else if let Some(idx) = name[pos..].find(part) {
            pos += idx + part.len();
        } else {
            return false;
        }
    }
    true
}

fn cmd_add(
    config: &Config,
    raw_source: &str,
    name: Option<String>,
    lang: Option<String>,
    version: Option<String>,
    local: bool,
) -> Result<()> {
    let source = Source::from_raw(raw_source, name, lang, version);
    let mut source_version = source
        .version
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let language = source.detected_language().unwrap_or("unknown").to_string();

    eprintln!("Extracting graph from {}...", source.name);

    // Use tree-sitter graph extraction
    let (file_graph, source_kind, origin, fingerprint) = match &source.kind {
        SourceKind::LocalPath(path) => {
            let fg =
                graph::extract::extract_dir(path, &source.name, &source_version, Some(&language))?;
            let fp = crate::fingerprint::fingerprint_dir(path).ok();
            (fg, "path", path.to_str().map(String::from), fp)
        }
        SourceKind::File(path) => {
            let fg =
                graph::extract::extract_file(path, &source.name, &source_version, Some(&language))?;
            let fp = crate::fingerprint::fingerprint_file(path).ok();
            (fg, "file", path.to_str().map(String::from), fp)
        }
        SourceKind::Crate(crate_name) => {
            let version_str = source.version.as_deref().unwrap_or("latest");
            let (dir, resolved_version) =
                crate::source::crate_download::download_crate(crate_name, version_str)?;
            source_version = resolved_version.clone();
            let fg =
                graph::extract::extract_dir(&dir, &source.name, &source_version, Some("rust"))?;
            (
                fg,
                "crate",
                Some(crate_name.clone()),
                Some(resolved_version),
            )
        }
        SourceKind::Url(_) => anyhow::bail!("URL sources not yet supported for graph extraction"),
    };

    eprintln!(
        "Extracted {} symbols, {} edges",
        file_graph.nodes.len(),
        file_graph.edges.len()
    );

    if file_graph.nodes.is_empty() {
        eprintln!("No symbols found.");
        return Ok(());
    }

    // Store in graph database
    let store_path = config.resolve_store_path(StoreScope::from_flags(local, false));
    let store = GraphStore::open(&store_path)?;
    store.upsert_source(
        &source.name,
        &source_version,
        &language,
        &file_graph.nodes,
        &file_graph.edges,
    )?;
    store.set_source_meta(
        &source.name,
        source_kind,
        origin.as_deref(),
        fingerprint.as_deref(),
    )?;

    eprintln!(
        "Indexed {} symbols from {} into {}",
        file_graph.nodes.len(),
        source.name,
        store_path.display()
    );

    Ok(())
}

fn cmd_query(
    config: &Config,
    query: &str,
    also: &[String],
    top: usize,
    source: Option<&str>,
    format: &str,
    local: bool,
    global: bool,
    db: Option<&std::path::Path>,
) -> Result<()> {
    let store_path = if let Some(path) = db {
        path.to_path_buf()
    } else {
        config.resolve_store_path(StoreScope::from_flags(local, global))
    };

    if !store_path.exists() {
        anyhow::bail!("no index found at {}", store_path.display());
    }

    if db.is_some() {
        crate::artifact::check_artifact_compatibility(&store_path)?;
    }

    let store = GraphStore::open(&store_path)?;
    let result = if also.is_empty() {
        store.search_scoped(query, top, source)?
    } else {
        let mut queries = vec![query.to_string()];
        queries.extend(also.iter().cloned());
        store.search_multi(&queries, top, source)?
    };

    if result.nodes.is_empty() {
        eprintln!("No results found.");
        return Ok(());
    }

    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&search_result_to_json(&result))?
            );
        }
        "skeleton" => {
            // Compact, deterministic, prompt-prefix-ready block for use as a
            // one-shot context preprocessor (iyi): inject roux's ranked hits
            // into an agent's prompt prefix instead of exposing a live tool.
            // Measured to cut a capable agent's input tokens ~20-40% with no
            // accuracy loss. Fields kept minimal on purpose (no edges/scores/
            // bodies — all measured neutral-to-harmful); the block is stable
            // run-to-run so it caches in the prompt prefix.
            print!("{}", render_skeleton(&result));
        }
        _ => {
            // Print matched symbols first, then neighborhood
            for sym in &result.nodes {
                let is_match = result.matched_ids.contains(&sym.id);
                let marker = if is_match { "●" } else { "○" };
                let kind_str = &sym.kind;
                let score = result
                    .scores
                    .get(&sym.id)
                    .map(|s| format!(" [{:.4}]", s))
                    .unwrap_or_default();

                println!(
                    "{marker} {} ({kind_str}){score} {}:{}",
                    sym.qualified_name, sym.file_path, sym.start_line
                );

                if let Some(ref sig) = sym.signature {
                    println!("  {sig}");
                }
                if let Some(ref doc) = sym.doc {
                    let first_line = doc.lines().next().unwrap_or("");
                    if !first_line.is_empty() {
                        println!("  {first_line}");
                    }
                }

                // Show edges from this symbol
                let outgoing: Vec<_> = result
                    .edges
                    .iter()
                    .filter(|e| e.from_id == sym.id)
                    .collect();
                for edge in &outgoing {
                    if let Some(target) = result.nodes.iter().find(|s| s.id == edge.to_id) {
                        println!("  → {} {} ({})", edge.kind, target.name, target.kind);
                    }
                }
                println!();
            }
        }
    }

    Ok(())
}

/// Render a search result as a compact `--format skeleton` block: one entry per
/// ranked symbol with `qualified_name (file:line)`, signature, and a one-line
/// doc truncated to ~160 chars. Deterministic given a fixed result (no scores or
/// timestamps) so the block is stable across runs and caches in a prompt prefix.
pub fn render_skeleton(result: &crate::graph::store::SearchResult) -> String {
    const DOC_MAX: usize = 160;
    let mut out = String::new();
    for sym in &result.nodes {
        out.push_str(&format!(
            "- {} ({}:{})\n",
            sym.qualified_name, sym.file_path, sym.start_line
        ));
        if let Some(sig) = sym
            .signature
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            out.push_str(&format!("    {sig}\n"));
        }
        if let Some(doc) = sym.doc.as_deref() {
            let flat = doc.split_whitespace().collect::<Vec<_>>().join(" ");
            if !flat.is_empty() {
                let trimmed = if flat.chars().count() > DOC_MAX {
                    let cut: String = flat.chars().take(DOC_MAX).collect();
                    format!("{cut}…")
                } else {
                    flat
                };
                out.push_str(&format!("    // {trimmed}\n"));
            }
        }
    }
    out
}

/// Staleness verdict for an indexed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Indexed content still matches the origin.
    Fresh,
    /// Origin has drifted — re-index needed. Carries a short reason.
    Stale(String),
    /// We can't check (e.g. origin path isn't local, or source_kind is a crate).
    Unknown,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Fresh => "fresh",
            Status::Stale(_) => "stale",
            Status::Unknown => "?",
        }
    }
    fn reason(&self) -> Option<&str> {
        match self {
            Status::Stale(r) => Some(r.as_str()),
            _ => None,
        }
    }
}

/// Check staleness using only local filesystem signals. Crate/URL sources
/// return Unknown — their upstream-staleness check lives in `roux sync`.
pub fn check_source_status(record: &crate::graph::store::SourceRecord) -> Status {
    match record.source_kind.as_str() {
        "path" => {
            let Some(origin) = record.origin.as_deref() else {
                return Status::Unknown;
            };
            let path = std::path::Path::new(origin);
            if !path.exists() {
                return Status::Unknown;
            }
            let Some(stored) = record.fingerprint.as_deref() else {
                return Status::Unknown;
            };
            match crate::fingerprint::fingerprint_dir(path) {
                Ok(current) if current == stored => Status::Fresh,
                Ok(_) => Status::Stale("content changed".into()),
                Err(_) => Status::Unknown,
            }
        }
        "file" => {
            let Some(origin) = record.origin.as_deref() else {
                return Status::Unknown;
            };
            let path = std::path::Path::new(origin);
            if !path.exists() {
                return Status::Unknown;
            }
            let Some(stored) = record.fingerprint.as_deref() else {
                return Status::Unknown;
            };
            match crate::fingerprint::fingerprint_file(path) {
                Ok(current) if current == stored => Status::Fresh,
                Ok(_) => Status::Stale("content changed".into()),
                Err(_) => Status::Unknown,
            }
        }
        _ => Status::Unknown,
    }
}

fn cmd_list(
    config: &Config,
    format: &str,
    local: bool,
    global: bool,
    db: Option<&std::path::Path>,
) -> Result<()> {
    let local_path = std::path::PathBuf::from(".roux/db.sqlite");

    let mut rows: Vec<(crate::graph::store::SourceRecord, Status, String)> = Vec::new();

    if let Some(path) = db {
        if !path.exists() {
            anyhow::bail!("no index found at {}", path.display());
        }
        crate::artifact::check_artifact_compatibility(path)?;
        let store = GraphStore::open(path)?;
        for src in store.list_sources()? {
            let status = check_source_status(&src);
            let display_name = src.name.clone();
            rows.push((src, status, display_name));
        }
    } else {
        let scope = StoreScope::from_flags(local, global);
        let include_local = matches!(scope, StoreScope::Local | StoreScope::Auto);
        let include_global = matches!(scope, StoreScope::Global | StoreScope::Auto);
        let global_path = config.resolve_store_path(StoreScope::Global);

        if include_local && local_path.exists() {
            let store = GraphStore::open(&local_path)?;
            for src in store.list_sources()? {
                let status = check_source_status(&src);
                let display_name = format!("{} (local)", src.name);
                rows.push((src, status, display_name));
            }
        }

        if include_global && global_path.exists() && global_path != local_path {
            let store = GraphStore::open(&global_path)?;
            for src in store.list_sources()? {
                let status = check_source_status(&src);
                let display_name = src.name.clone();
                rows.push((src, status, display_name));
            }
        }
    }

    if rows.is_empty() {
        eprintln!("No indexed sources.");
        return Ok(());
    }

    match format {
        "json" => {
            println!(
                "{}",
                serde_json::to_string_pretty(&list_rows_to_json(&rows))?
            );
        }
        _ => {
            println!(
                "{:<22} {:<12} {:<10} {:>8}  STATUS",
                "SOURCE", "VERSION", "LANGUAGE", "SYMBOLS"
            );
            println!("{}", "─".repeat(66));
            for (src, status, display) in &rows {
                let status_cell = match status {
                    Status::Stale(r) => format!("stale ({r})"),
                    _ => status.label().to_string(),
                };
                println!(
                    "{:<22} {:<12} {:<10} {:>8}  {}",
                    display, src.version, src.language, src.node_count, status_cell
                );
            }
        }
    }

    Ok(())
}

fn cmd_serve(
    config: &Config,
    local: bool,
    global: bool,
    db: Option<&std::path::Path>,
) -> Result<()> {
    let store_path = if let Some(path) = db {
        if !path.exists() {
            anyhow::bail!("no index found at {}", path.display());
        }
        crate::artifact::check_artifact_compatibility(path)?;
        path.to_path_buf()
    } else {
        let resolved = config.resolve_store_path(StoreScope::from_flags(local, global));
        if !resolved.exists() {
            anyhow::bail!(
                "no index found at {}. Run `roux init` first.",
                resolved.display()
            );
        }
        resolved
    };

    eprintln!("roux MCP server: serving {}", store_path.display());
    crate::mcp::run_stdio(store_path)
}

fn cmd_export(config: &Config, output: &std::path::Path, gzip: bool, global: bool) -> Result<()> {
    let source_db = if global {
        config.resolve_store_path(StoreScope::Global)
    } else {
        // Auto honors prefer_local if .roux/db.sqlite exists, else falls back to global.
        config.resolve_store_path(StoreScope::Auto)
    };

    if !source_db.exists() {
        anyhow::bail!(
            "no index found at {}. Run `roux init --local` or `roux add` first.",
            source_db.display()
        );
    }

    eprintln!("Exporting {} → {}", source_db.display(), output.display());
    let written = crate::artifact::export(&source_db, output, gzip)?;
    let size = std::fs::metadata(&written).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "Wrote {} ({:.1} MiB){}",
        written.display(),
        size as f64 / 1024.0 / 1024.0,
        if gzip { ", gzipped" } else { "" }
    );
    Ok(())
}

/// Per-source plan produced during `roux sync`.
#[derive(Debug)]
enum SyncPlan {
    Fresh,
    Stale { reason: String, action: SyncAction },
    Unknown(String),
}

#[derive(Debug)]
enum SyncAction {
    /// Re-download and re-extract a crate at a new version.
    Crate { new_version: String },
    /// Re-walk a directory source and re-index.
    Path,
    /// Re-read and re-index a single file source.
    File,
}

fn plan_sync(record: &crate::graph::store::SourceRecord, expected: Option<&str>) -> SyncPlan {
    match record.source_kind.as_str() {
        "crate" => match expected {
            None => SyncPlan::Unknown("no lockfile entry".into()),
            Some(v) if v == record.version => SyncPlan::Fresh,
            Some(v) => SyncPlan::Stale {
                reason: format!("{} → {v}", record.version),
                action: SyncAction::Crate {
                    new_version: v.to_string(),
                },
            },
        },
        "path" => match check_source_status(record) {
            Status::Fresh => SyncPlan::Fresh,
            Status::Stale(r) => SyncPlan::Stale {
                reason: r,
                action: SyncAction::Path,
            },
            Status::Unknown => SyncPlan::Unknown("origin path unavailable".into()),
        },
        "file" => match check_source_status(record) {
            Status::Fresh => SyncPlan::Fresh,
            Status::Stale(r) => SyncPlan::Stale {
                reason: r,
                action: SyncAction::File,
            },
            Status::Unknown => SyncPlan::Unknown("origin file unavailable".into()),
        },
        "" => SyncPlan::Unknown("no source metadata".into()),
        other => SyncPlan::Unknown(format!("unsupported source kind: {other}")),
    }
}

fn cmd_sync(config: &Config, source_filter: Option<&str>, dry_run: bool) -> Result<()> {
    use std::collections::HashMap;

    let cwd = std::env::current_dir()?;
    let project = crate::lockfile::detect_project(&cwd);

    let store_path = config.resolve_store_path(StoreScope::Auto);
    if !store_path.exists() {
        anyhow::bail!(
            "no index found at {}. Run `roux init` or `roux add` first.",
            store_path.display()
        );
    }

    let store = GraphStore::open(&store_path)?;
    let sources = store.list_sources()?;

    // Map crate name → expected version from the lockfile (if any). The
    // persisted `origin` holds the crate name; fall back to `name` for legacy
    // sources written before staleness metadata existed.
    let expected: HashMap<String, String> = project
        .as_ref()
        .map(|p| {
            p.deps
                .iter()
                .filter_map(|d| d.version.clone().map(|v| (d.name.clone(), v)))
                .collect()
        })
        .unwrap_or_default();

    // Classify every source.
    let mut plans: Vec<(crate::graph::store::SourceRecord, SyncPlan)> = Vec::new();
    for src in sources {
        if let Some(f) = source_filter
            && src.name != f
        {
            continue;
        }
        let expected_ver = src
            .origin
            .as_deref()
            .or(Some(src.name.as_str()))
            .and_then(|key| expected.get(key).map(String::as_str));
        let plan = plan_sync(&src, expected_ver);
        plans.push((src, plan));
    }

    if plans.is_empty() {
        if let Some(f) = source_filter {
            anyhow::bail!("source '{f}' not found in {}", store_path.display());
        }
        eprintln!("No sources indexed.");
        return Ok(());
    }

    // Report.
    let mut fresh = 0;
    let mut stale = 0;
    let mut unknown = 0;
    for (src, plan) in &plans {
        match plan {
            SyncPlan::Fresh => {
                fresh += 1;
                eprintln!("  {:<24} {:<6} fresh", src.name, src.source_kind);
            }
            SyncPlan::Stale { reason, .. } => {
                stale += 1;
                eprintln!(
                    "  {:<24} {:<6} stale  — {reason}",
                    src.name, src.source_kind
                );
            }
            SyncPlan::Unknown(why) => {
                unknown += 1;
                eprintln!("  {:<24} {:<6} ?      ({why})", src.name, src.source_kind);
            }
        }
    }
    eprintln!("\n{fresh} fresh, {stale} stale, {unknown} unknown");

    if stale == 0 {
        return Ok(());
    }
    if dry_run {
        eprintln!("(dry-run — skipping re-ingest)");
        return Ok(());
    }

    // Re-ingest stale sources.
    let timeout = std::time::Duration::from_secs(DEFAULT_CRATE_TIMEOUT_SECS);
    let mut updated = 0;
    let mut failed = 0;
    eprintln!("\nSyncing {stale} source(s)...");
    for (src, plan) in &plans {
        let SyncPlan::Stale { action, .. } = plan else {
            continue;
        };
        match action {
            SyncAction::Crate { new_version } => {
                let crate_name = src.origin.as_deref().unwrap_or(&src.name);
                eprint!("  {crate_name} v{new_version} ... ");
                match extract_crate_with_timeout(crate_name, new_version, timeout) {
                    CrateOutcome::Ok { version, graph } => {
                        let count = graph.nodes.len();
                        if let Err(e) = store
                            .upsert_source(&src.name, &version, "rust", &graph.nodes, &graph.edges)
                            .and_then(|()| {
                                store.set_source_meta(
                                    &src.name,
                                    "crate",
                                    Some(crate_name),
                                    Some(&version),
                                )
                            })
                        {
                            eprintln!("failed: {e}");
                            failed += 1;
                        } else {
                            eprintln!("{count} symbols");
                            updated += 1;
                        }
                    }
                    CrateOutcome::Err(e) => {
                        eprintln!("failed: {e}");
                        failed += 1;
                    }
                    CrateOutcome::Timeout => {
                        eprintln!("timeout");
                        failed += 1;
                    }
                }
            }
            SyncAction::Path => {
                let Some(origin) = src.origin.as_deref() else {
                    eprintln!("  {} (path)  skip — origin missing", src.name);
                    failed += 1;
                    continue;
                };
                let path = std::path::Path::new(origin);
                eprint!("  {} (path) ... ", src.name);
                match graph::extract::extract_dir(
                    path,
                    &src.name,
                    &src.version,
                    Some(&src.language),
                ) {
                    Ok(fg) => {
                        let fp = crate::fingerprint::fingerprint_dir(path).ok();
                        match store
                            .upsert_source(
                                &src.name,
                                &src.version,
                                &src.language,
                                &fg.nodes,
                                &fg.edges,
                            )
                            .and_then(|()| {
                                store.set_source_meta(
                                    &src.name,
                                    "path",
                                    Some(origin),
                                    fp.as_deref(),
                                )
                            }) {
                            Ok(()) => {
                                eprintln!("{} symbols", fg.nodes.len());
                                updated += 1;
                            }
                            Err(e) => {
                                eprintln!("failed: {e}");
                                failed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("failed: {e}");
                        failed += 1;
                    }
                }
            }
            SyncAction::File => {
                let Some(origin) = src.origin.as_deref() else {
                    eprintln!("  {} (file)  skip — origin missing", src.name);
                    failed += 1;
                    continue;
                };
                let path = std::path::Path::new(origin);
                eprint!("  {} (file) ... ", src.name);
                match graph::extract::extract_file(
                    path,
                    &src.name,
                    &src.version,
                    Some(&src.language),
                ) {
                    Ok(fg) => {
                        let fp = crate::fingerprint::fingerprint_file(path).ok();
                        match store
                            .upsert_source(
                                &src.name,
                                &src.version,
                                &src.language,
                                &fg.nodes,
                                &fg.edges,
                            )
                            .and_then(|()| {
                                store.set_source_meta(
                                    &src.name,
                                    "file",
                                    Some(origin),
                                    fp.as_deref(),
                                )
                            }) {
                            Ok(()) => {
                                eprintln!("{} symbols", fg.nodes.len());
                                updated += 1;
                            }
                            Err(e) => {
                                eprintln!("failed: {e}");
                                failed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("failed: {e}");
                        failed += 1;
                    }
                }
            }
        }
    }

    eprintln!("\nDone: {updated} updated, {failed} failed");
    Ok(())
}

fn cmd_remove(config: &Config, source_name: &str) -> Result<()> {
    let store_path = config.resolve_store_path(StoreScope::Auto);
    if !store_path.exists() {
        anyhow::bail!("no index found at {}", store_path.display());
    }

    let store = GraphStore::open(&store_path)?;
    store.remove_source(source_name)?;
    eprintln!("Removed {source_name} from index");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_add() {
        let cli = Cli::try_parse_from(["roux", "add", "tokio"]).unwrap();
        assert!(matches!(cli.command, Command::Add { ref source, .. } if source == "tokio"));
    }

    #[test]
    fn test_parse_add_with_options() {
        Cli::try_parse_from([
            "roux",
            "add",
            "tokio",
            "--lang",
            "rust",
            "--local",
            "--version",
            "1.35",
            "--name",
            "my-tokio",
        ])
        .unwrap();
    }

    #[test]
    fn test_parse_query() {
        Cli::try_parse_from(["roux", "query", "how to spawn"]).unwrap();
    }

    #[test]
    fn test_parse_query_with_options() {
        Cli::try_parse_from([
            "roux",
            "query",
            "mutex lock",
            "--top",
            "5",
            "--source",
            "tokio",
            "--format",
            "json",
        ])
        .unwrap();
    }

    #[test]
    fn test_parse_init() {
        Cli::try_parse_from(["roux", "init"]).unwrap();
        Cli::try_parse_from(["roux", "init", "--transitive", "--local"]).unwrap();
    }

    #[test]
    fn test_parse_init_exclude_and_timeout() {
        let cli = Cli::try_parse_from([
            "roux",
            "init",
            "--exclude",
            "candle*",
            "--exclude",
            "*-sys",
            "--timeout",
            "15",
        ])
        .unwrap();
        match cli.command {
            Command::Init {
                exclude, timeout, ..
            } => {
                assert_eq!(exclude, vec!["candle*", "*-sys"]);
                assert_eq!(timeout, 15);
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn test_parse_init_default_timeout() {
        let cli = Cli::try_parse_from(["roux", "init"]).unwrap();
        match cli.command {
            Command::Init {
                exclude, timeout, ..
            } => {
                assert!(exclude.is_empty());
                assert_eq!(timeout, DEFAULT_CRATE_TIMEOUT_SECS);
            }
            _ => panic!("expected Init"),
        }
    }

    #[test]
    fn test_matches_glob_exact() {
        assert!(matches_glob("tokio", "tokio"));
        assert!(matches_glob("Tokio", "tokio")); // case-insensitive
        assert!(!matches_glob("tokio", "tokio-util"));
    }

    #[test]
    fn test_matches_glob_prefix() {
        assert!(matches_glob("candle*", "candle"));
        assert!(matches_glob("candle*", "candle-core"));
        assert!(matches_glob("candle*", "candle-nn"));
        assert!(!matches_glob("candle*", "foo-candle"));
    }

    #[test]
    fn test_matches_glob_suffix() {
        assert!(matches_glob("*-sys", "libc-sys"));
        assert!(matches_glob("*-sys", "-sys"));
        assert!(!matches_glob("*-sys", "libc-system"));
    }

    #[test]
    fn test_matches_glob_contains() {
        assert!(matches_glob("*candle*", "candle"));
        assert!(matches_glob("*candle*", "foo-candle-core"));
        assert!(!matches_glob("*candle*", "torch-core"));
    }

    #[test]
    fn test_matches_glob_middle() {
        assert!(matches_glob("foo*bar", "foobar"));
        assert!(matches_glob("foo*bar", "foo-xyz-bar"));
        assert!(!matches_glob("foo*bar", "foo-xyz"));
        assert!(!matches_glob("foo*bar", "bar-foo"));
    }

    fn make_record(
        kind: &str,
        origin: Option<&str>,
        fingerprint: Option<&str>,
    ) -> crate::graph::store::SourceRecord {
        crate::graph::store::SourceRecord {
            name: "test".into(),
            version: "1.0".into(),
            language: "rust".into(),
            ingested_at: 0,
            source_kind: kind.into(),
            origin: origin.map(String::from),
            fingerprint: fingerprint.map(String::from),
            node_count: 0,
        }
    }

    #[test]
    fn test_status_unknown_for_crate() {
        let rec = make_record("crate", Some("serde"), Some("1.0.200"));
        assert_eq!(check_source_status(&rec), Status::Unknown);
    }

    #[test]
    fn test_status_unknown_when_origin_missing() {
        let rec = make_record("path", Some("/nonexistent/path/xyz"), Some("deadbeef"));
        assert_eq!(check_source_status(&rec), Status::Unknown);
    }

    #[test]
    fn test_status_fresh_and_stale_for_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        let fp = crate::fingerprint::fingerprint_dir(tmp.path()).unwrap();
        let origin = tmp.path().to_str().unwrap();

        let fresh = make_record("path", Some(origin), Some(&fp));
        assert_eq!(check_source_status(&fresh), Status::Fresh);

        let wrong = make_record("path", Some(origin), Some("0000000000"));
        assert!(matches!(check_source_status(&wrong), Status::Stale(_)));
    }

    #[test]
    fn test_status_fresh_and_stale_for_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("x.txt");
        std::fs::write(&p, "hello").unwrap();
        let fp = crate::fingerprint::fingerprint_file(&p).unwrap();

        let fresh = make_record("file", p.to_str(), Some(&fp));
        assert_eq!(check_source_status(&fresh), Status::Fresh);

        std::fs::write(&p, "goodbye").unwrap();
        let stale = make_record("file", p.to_str(), Some(&fp));
        assert!(matches!(check_source_status(&stale), Status::Stale(_)));
    }

    #[test]
    fn test_plan_sync_crate_fresh() {
        let rec = make_record("crate", Some("serde"), Some("1.0.200"));
        let mut r = rec.clone();
        r.version = "1.0.200".into();
        assert!(matches!(plan_sync(&r, Some("1.0.200")), SyncPlan::Fresh));
    }

    #[test]
    fn test_plan_sync_crate_stale() {
        let mut r = make_record("crate", Some("serde"), Some("1.0.200"));
        r.version = "1.0.200".into();
        match plan_sync(&r, Some("1.0.210")) {
            SyncPlan::Stale { reason, action } => {
                assert!(reason.contains("1.0.200"));
                assert!(reason.contains("1.0.210"));
                assert!(
                    matches!(action, SyncAction::Crate { ref new_version } if new_version == "1.0.210")
                );
            }
            _ => panic!("expected Stale"),
        }
    }

    #[test]
    fn test_plan_sync_crate_no_lockfile_entry() {
        let r = make_record("crate", Some("serde"), Some("1.0.200"));
        assert!(matches!(plan_sync(&r, None), SyncPlan::Unknown(_)));
    }

    #[test]
    fn test_plan_sync_path_fresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        let fp = crate::fingerprint::fingerprint_dir(tmp.path()).unwrap();
        let rec = make_record("path", tmp.path().to_str(), Some(&fp));
        assert!(matches!(plan_sync(&rec, None), SyncPlan::Fresh));
    }

    #[test]
    fn test_plan_sync_path_stale() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}").unwrap();
        let rec = make_record("path", tmp.path().to_str(), Some("wrongfingerprint"));
        assert!(matches!(plan_sync(&rec, None), SyncPlan::Stale { .. }));
    }

    #[test]
    fn test_plan_sync_unknown_kind() {
        let rec = make_record("url", Some("https://x"), None);
        assert!(matches!(plan_sync(&rec, None), SyncPlan::Unknown(_)));
    }

    #[test]
    fn test_parse_list() {
        Cli::try_parse_from(["roux", "list"]).unwrap();
        Cli::try_parse_from(["roux", "list", "--format", "json"]).unwrap();
        Cli::try_parse_from(["roux", "list", "--local"]).unwrap();
        Cli::try_parse_from(["roux", "list", "--global"]).unwrap();
    }

    #[test]
    fn test_parse_local_global_conflict() {
        // --local and --global are mutually exclusive on both query and list.
        assert!(Cli::try_parse_from(["roux", "query", "x", "--local", "--global"]).is_err());
        assert!(Cli::try_parse_from(["roux", "list", "--local", "--global"]).is_err());
    }

    #[test]
    fn test_parse_sync() {
        Cli::try_parse_from(["roux", "sync"]).unwrap();
        Cli::try_parse_from(["roux", "sync", "tokio"]).unwrap();
        Cli::try_parse_from(["roux", "sync", "--dry-run"]).unwrap();
    }

    #[test]
    fn test_parse_remove() {
        Cli::try_parse_from(["roux", "remove", "tokio"]).unwrap();
    }

    #[test]
    fn test_parse_query_with_db() {
        let cli =
            Cli::try_parse_from(["roux", "query", "auth", "--db", "/tmp/index.sqlite"]).unwrap();
        match cli.command {
            Command::Query { db, .. } => {
                assert_eq!(db.unwrap(), std::path::PathBuf::from("/tmp/index.sqlite"));
            }
            _ => panic!("expected Query"),
        }
    }

    #[test]
    fn test_parse_list_with_db() {
        let cli = Cli::try_parse_from(["roux", "list", "--db", "./x.sqlite"]).unwrap();
        assert!(matches!(cli.command, Command::List { db: Some(_), .. }));
    }

    #[test]
    fn test_parse_export() {
        let cli = Cli::try_parse_from(["roux", "export", "--output", "my-index.sqlite"]).unwrap();
        match cli.command {
            Command::Export {
                output,
                gzip,
                global,
            } => {
                assert_eq!(output, std::path::PathBuf::from("my-index.sqlite"));
                assert!(!gzip);
                assert!(!global);
            }
            _ => panic!("expected Export"),
        }
    }

    #[test]
    fn test_parse_export_gzipped() {
        let cli = Cli::try_parse_from([
            "roux",
            "export",
            "--output",
            "my.sqlite.gz",
            "--gzip",
            "--global",
        ])
        .unwrap();
        match cli.command {
            Command::Export { gzip, global, .. } => {
                assert!(gzip);
                assert!(global);
            }
            _ => panic!("expected Export"),
        }
    }

    #[test]
    fn test_parse_no_args_fails() {
        assert!(Cli::try_parse_from(["roux"]).is_err());
    }

    #[test]
    fn test_parse_unknown_command_fails() {
        assert!(Cli::try_parse_from(["roux", "unknown"]).is_err());
    }
}
