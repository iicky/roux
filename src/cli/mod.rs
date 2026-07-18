use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use crate::config::{Config, StoreScope};
use crate::graph;
use crate::graph::extract::FileGraph;
use crate::graph::store::GraphStore;
use crate::output;
use crate::source::Source;
use crate::source::SourceKind;

const DEFAULT_CRATE_TIMEOUT_SECS: u64 = 30;

/// Default number of results for a query when `--top` is not given, shared by
/// the CLI and the MCP tool so the two never diverge.
pub const DEFAULT_TOP_K: usize = 5;

/// Output format for `roux query`. A validated enum so an unknown value
/// (e.g. `--format josn`) is a hard parse error, not a silent fall-through.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum QueryFormat {
    /// Human-readable ranked list (default).
    Text,
    /// Machine-readable JSON envelope.
    Json,
    /// Deterministic prompt-prefix block for one-shot context injection.
    Skeleton,
    /// Budgeted ranked matches plus neighbor names for a live tool.
    Compact,
}

/// Output format for `roux list`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ListFormat {
    /// Human-readable table (default).
    Text,
    /// Machine-readable JSON.
    Json,
}

#[derive(Parser)]
#[command(name = "roux", version, about = "the base your coding agents build on")]
pub struct Cli {
    /// Suppress routine status (progress, completions); warnings and errors still show
    #[arg(long, global = true, conflicts_with = "verbose")]
    quiet: bool,
    /// Show verbose detail output
    #[arg(long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Detect project type and ingest docs for all dependencies into a
    /// project-local .roux/db.sqlite (pass --global for the shared store)
    Init {
        /// Include transitive dependencies
        #[arg(long)]
        transitive: bool,
        /// Write to the project-local .roux/db.sqlite (this is the default)
        #[arg(long, conflicts_with = "global")]
        local: bool,
        /// Write to the shared global index instead of .roux/db.sqlite
        #[arg(long)]
        global: bool,
        /// Skip dependencies matching a glob pattern (repeatable, e.g. --exclude 'candle*')
        #[arg(long = "exclude", value_name = "PATTERN")]
        exclude: Vec<String>,
        /// Per-crate extraction timeout in seconds
        #[arg(long, default_value_t = DEFAULT_CRATE_TIMEOUT_SECS)]
        timeout: u64,
    },
    /// Ingest a source into the index
    Add {
        /// Sources: crate names or local paths (directory or file), space-separated
        #[arg(required = true, num_args = 1.., value_name = "SOURCE")]
        sources: Vec<String>,
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
        #[arg(long, default_value_t = DEFAULT_TOP_K)]
        top: usize,
        /// Restrict search to a named source
        #[arg(long)]
        source: Option<String>,
        /// Output format: text, json, skeleton (one-shot prompt-prefix block),
        /// or compact (budgeted ranked matches + neighbor names for live use)
        #[arg(long, default_value = "text")]
        format: QueryFormat,
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
        format: ListFormat,
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
    /// Incrementally refresh path/file sources from the working tree
    Update {
        /// Update a specific source
        source: Option<String>,
        /// Update the local index only (mutually exclusive with --global)
        #[arg(long, conflicts_with = "global")]
        local: bool,
        /// Update the global index only (mutually exclusive with --local)
        #[arg(long)]
        global: bool,
        /// Show what would change without applying
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
    /// Generate a shell completion script for your shell
    Completions {
        /// Shell to generate completions for (bash, zsh, fish, powershell, elvish)
        shell: clap_complete::Shell,
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
        let level = if self.quiet {
            output::Level::Quiet
        } else if self.verbose {
            output::Level::Verbose
        } else {
            output::Level::Normal
        };
        output::init(level);

        // Completions generation is pure stdout and must work even when config
        // or the home directory is unavailable, so handle it before loading config.
        if let Command::Completions { shell } = &self.command {
            return cmd_completions(*shell);
        }

        let config = Config::load()?;
        match &self.command {
            Command::Init {
                transitive,
                global,
                exclude,
                timeout,
                ..
            } => cmd_init(&config, *transitive, *global, exclude, *timeout),
            Command::Add {
                sources,
                lang,
                local,
                version,
                name,
            } => cmd_add(
                &config,
                sources,
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
                *format,
                *local,
                *global,
                db.as_deref(),
            ),
            Command::List {
                format,
                local,
                global,
                db,
            } => cmd_list(&config, *format, *local, *global, db.as_deref()),
            Command::Serve { local, global, db } => {
                cmd_serve(&config, *local, *global, db.as_deref())
            }
            Command::Sync { source, dry_run } => cmd_sync(&config, source.as_deref(), *dry_run),
            Command::Update {
                source,
                local,
                global,
                dry_run,
            } => cmd_update(&config, source.as_deref(), *local, *global, *dry_run),
            Command::Remove { source } => cmd_remove(&config, source),
            Command::Export {
                output,
                gzip,
                global,
            } => cmd_export(&config, output, *gzip, *global),
            Command::Completions { .. } => {
                unreachable!("completions handled before config load")
            }
        }
    }
}

/// Write a shell completion script for `shell` to stdout (data, not status).
fn cmd_completions(shell: clap_complete::Shell) -> Result<()> {
    let mut cmd = <Cli as CommandFactory>::command();
    clap_complete::generate(shell, &mut cmd, "roux", &mut std::io::stdout());
    Ok(())
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

/// JSON shape for `roux list`, produced by the CLI list handler.
fn list_rows_to_json(
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
    global: bool,
    exclude: &[String],
    timeout_secs: u64,
) -> Result<()> {
    let cwd = std::env::current_dir()?;

    // Init is a project-scoped operation, so it writes to the project-local
    // .roux/db.sqlite by default; --global opts into the shared store. A bare
    // `roux init` must never silently pollute the global store with this repo.
    let scope = if global {
        StoreScope::Global
    } else {
        StoreScope::Local
    };
    let store_path = config.resolve_store_path(scope);
    if let Some(parent) = store_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let store = GraphStore::open(&store_path)?;

    let timeout = Duration::from_secs(timeout_secs);
    index_project(&cwd, &store, transitive, exclude, timeout)?;
    output::done(format!(
        "init complete — indexed to {}",
        store_path.display()
    ));
    Ok(())
}

/// Ingest a project into `store`: dependencies when a manifest is present, plus
/// the local source in every case. A repo without a lockfile/manifest — a C++
/// firmware tree, a monorepo root with no root deps — still gets its own source
/// indexed rather than erroring out or silently indexing nothing.
fn index_project(
    dir: &Path,
    store: &GraphStore,
    transitive: bool,
    exclude: &[String],
    timeout: Duration,
) -> Result<()> {
    let project = crate::lockfile::detect_project(dir);

    // 1. Dependencies — only when a manifest is present.
    if let Some(project) = &project {
        let direct_count = project.deps.iter().filter(|d| d.direct).count();
        output::step(format!(
            "Detected {:?} project ({} direct deps, {} total) from {}",
            project.kind,
            direct_count,
            project.deps.len(),
            project
                .lockfile
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        ));

        let deps: Vec<&crate::lockfile::Dependency> = if transitive {
            project.deps.iter().collect()
        } else {
            project.deps.iter().filter(|d| d.direct).collect()
        };

        if deps.is_empty() {
            // Not an error: a monorepo root or an app with no declared deps
            // still has local source worth indexing below.
            output::step("No dependencies to ingest");
        } else {
            output::step(format!("Ingesting {} dependencies", deps.len()));
            ingest_deps(&deps, project.kind, exclude, timeout, store);
        }
    } else {
        output::step("No lockfile or manifest found — indexing local source only");
    }

    // 2. Local source — always. `None` language hint so each file is parsed by
    // its own extension (a mixed/monorepo tree isn't forced to one language).
    let project_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let file_graph = {
        let _spin = output::spinner(format!("Indexing local source as '{project_name}'..."));
        graph::extract::extract_dir(dir, project_name, None)?
    };
    if file_graph.nodes.is_empty() {
        output::warn("No indexable source found");
    } else {
        // Conservative label: the project kind when known, else the dominant
        // language among the extracted symbols.
        let language = match &project {
            Some(p) => p.kind.language().to_string(),
            None => dominant_language(&file_graph.nodes),
        };
        store.upsert_source(
            project_name,
            "dev",
            &language,
            &file_graph.nodes,
            &file_graph.edges,
        )?;
        store.replace_files(project_name, &file_graph.files)?;
        let fp = crate::fingerprint::fingerprint_dir(dir).ok();
        store.set_source_meta(project_name, "path", dir.to_str(), fp.as_deref())?;
        output::ok(format!(
            "Indexed {} symbols, {} edges from local source",
            file_graph.nodes.len(),
            file_graph.edges.len()
        ));
    }

    Ok(())
}

/// Download + ingest each Rust crate dependency; other ecosystems have no
/// registry support yet and are skipped.
fn ingest_deps(
    deps: &[&crate::lockfile::Dependency],
    kind: crate::lockfile::ProjectKind,
    exclude: &[String],
    timeout: Duration,
    store: &GraphStore,
) {
    let mut success = 0;
    let mut excluded = 0;
    let mut skipped = 0;
    let mut failed = 0;

    for dep in deps {
        let version_str = dep.version.as_deref().unwrap_or("latest");

        if exclude.iter().any(|p| matches_glob(p, &dep.name)) {
            output::step(format!("{} (excluded)", dep.name));
            excluded += 1;
            continue;
        }

        // For Rust crates, download from crates.io
        if kind == crate::lockfile::ProjectKind::Rust {
            let spin = output::spinner(format!("{} v{version_str} ...", dep.name));
            let outcome = extract_crate_with_timeout(&dep.name, version_str, timeout);
            drop(spin);
            match outcome {
                CrateOutcome::Ok { version, graph } => {
                    let count = graph.nodes.len();
                    if count == 0 {
                        output::ok(format!("{} v{version} — 0 symbols", dep.name));
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
                            output::ok(format!("{} v{version} — {count} symbols", dep.name));
                            success += 1;
                        }
                        Err(e) => {
                            output::warn(format!("{} v{version} — failed: {e}", dep.name));
                            failed += 1;
                        }
                    }
                }
                CrateOutcome::Err(e) => {
                    output::warn(format!("{} — failed: {e}", dep.name));
                    failed += 1;
                }
                CrateOutcome::Timeout => {
                    output::warn(format!(
                        "{} — timeout after {}s",
                        dep.name,
                        timeout.as_secs()
                    ));
                    failed += 1;
                }
            }
        } else {
            // For other languages, we can only ingest local paths
            // TODO: add PyPI, npm registry support
            output::step(format!(
                "{} (skip — no registry support for {kind:?} yet)",
                dep.name
            ));
            skipped += 1;
        }
    }

    output::step(format!(
        "Deps: {success} ingested, {excluded} excluded, {skipped} skipped, {failed} failed"
    ));
}

/// Most frequent per-file language among extracted nodes; `"unknown"` if empty.
fn dominant_language(nodes: &[graph::Node]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in nodes {
        *counts.entry(n.language.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, c)| *c)
        .map(|(lang, _)| lang.to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
    // Extraction recurses over the syntax tree as deep as the source nests, so
    // this worker needs the same large stack `main` reserves — the default
    // ~2 MB thread stack overflows on deeply nested dependency ASTs.
    let spawn_result = thread::Builder::new()
        .stack_size(crate::settings::get().worker_stack_bytes)
        .spawn(move || {
            let result = (|| -> Result<(String, FileGraph)> {
                let (dir, resolved) =
                    crate::source::crate_download::download_crate(&name, &version)?;
                let graph = graph::extract::extract_dir(&dir, &name, Some("rust"))?;
                Ok((resolved, graph))
            })();
            let _ = tx.send(result);
        });
    if let Err(e) = spawn_result {
        return CrateOutcome::Err(anyhow::anyhow!("failed to spawn extraction worker: {e}"));
    }
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
    sources: &[String],
    name: Option<String>,
    lang: Option<String>,
    version: Option<String>,
    local: bool,
) -> Result<()> {
    // --name/--version rename or pin a single source; they cannot fan out across
    // a batch, so reject the ambiguous combination up front.
    if sources.len() > 1 && (name.is_some() || version.is_some()) {
        anyhow::bail!(
            "--name and --version apply to a single source; run one `roux add` per source to override them"
        );
    }

    // Open the store once and reuse it for every source in the batch.
    let store_path = config.resolve_store_path(StoreScope::from_flags(local, false));
    let store = GraphStore::open(&store_path)?;

    let mut indexed = 0usize;
    let mut failed = 0usize;
    for raw in sources {
        // name/version are single-source overrides, only ever set when len == 1.
        match add_one(&store, raw, name.clone(), lang.clone(), version.clone()) {
            Ok(0) => output::warn(format!("{raw} — no symbols found")),
            Ok(n) => {
                output::ok(format!("{raw} — {n} symbols"));
                indexed += 1;
            }
            Err(e) => {
                output::warn(format!("{raw} — failed: {e}"));
                failed += 1;
            }
        }
    }

    if failed > 0 {
        anyhow::bail!(
            "{failed} of {} source(s) failed to index (indexed {indexed})",
            sources.len()
        );
    }
    output::done(format!(
        "Added {indexed} source(s) to {}",
        store_path.display()
    ));
    Ok(())
}

/// Extract one source and write it into `store`; returns the indexed symbol
/// count (0 means nothing extractable was found).
fn add_one(
    store: &GraphStore,
    raw_source: &str,
    name: Option<String>,
    lang: Option<String>,
    version: Option<String>,
) -> Result<usize> {
    let source = Source::from_raw(raw_source, name, lang, version);
    let mut source_version = source
        .version
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    // The extraction hint is an Option: when detection is unsure, pass None so
    // extract_dir/extract_file fall back to their own per-file detection rather
    // than being pinned to a bogus "unknown" that fails as an unsupported lang.
    let hint = source.detected_language();
    let language = hint.unwrap_or("unknown").to_string();
    output::detail(format!(
        "{}: language hint {}",
        source.name,
        hint.unwrap_or("(auto-detect)")
    ));
    let _spin = output::spinner(format!("Extracting graph from {}...", source.name));

    let (file_graph, source_kind, origin, fingerprint) = match &source.kind {
        SourceKind::LocalPath(path) => {
            let fg = graph::extract::extract_dir(path, &source.name, hint)?;
            let fp = crate::fingerprint::fingerprint_dir(path).ok();
            (fg, "path", path.to_str().map(String::from), fp)
        }
        SourceKind::File(path) => {
            let fg = graph::extract::extract_file(path, &source.name, hint)?;
            let fp = crate::fingerprint::fingerprint_file(path).ok();
            (fg, "file", path.to_str().map(String::from), fp)
        }
        SourceKind::Crate(crate_name) => {
            let version_str = source.version.as_deref().unwrap_or("latest");
            let (dir, resolved_version) =
                crate::source::crate_download::download_crate(crate_name, version_str)?;
            source_version = resolved_version.clone();
            let fg = graph::extract::extract_dir(&dir, &source.name, Some("rust"))?;
            (
                fg,
                "crate",
                Some(crate_name.clone()),
                Some(resolved_version),
            )
        }
        SourceKind::Url(_) => anyhow::bail!("URL sources not yet supported for graph extraction"),
    };

    if file_graph.nodes.is_empty() {
        return Ok(0);
    }

    store.upsert_source(
        &source.name,
        &source_version,
        &language,
        &file_graph.nodes,
        &file_graph.edges,
    )?;
    store.replace_files(&source.name, &file_graph.files)?;
    store.set_source_meta(
        &source.name,
        source_kind,
        origin.as_deref(),
        fingerprint.as_deref(),
    )?;
    Ok(file_graph.nodes.len())
}

/// Open an existing index at `path`, or bail with an actionable message when it
/// is absent. `hint` is appended after the path (a "Run `roux …`" nudge, or "").
/// `is_artifact` runs the portable-artifact compatibility check first — the
/// `--db` path, where a schema mismatch needs a distinct, upgrade-oriented error.
fn open_existing_store(
    path: &std::path::Path,
    is_artifact: bool,
    hint: &str,
) -> Result<GraphStore> {
    if !path.exists() {
        anyhow::bail!("no index found at {}{hint}", path.display());
    }
    if is_artifact {
        crate::artifact::check_artifact_compatibility(path)?;
    }
    GraphStore::open(path)
}

fn cmd_query(
    config: &Config,
    query: &str,
    also: &[String],
    top: usize,
    source: Option<&str>,
    format: QueryFormat,
    local: bool,
    global: bool,
    db: Option<&std::path::Path>,
) -> Result<()> {
    let store_path = if let Some(path) = db {
        path.to_path_buf()
    } else {
        config.resolve_store_path(StoreScope::from_flags(local, global))
    };

    let store = open_existing_store(
        &store_path,
        db.is_some(),
        ". Run `roux init` or `roux add` first.",
    )?;
    // Validate --source up front so an unknown name gives an actionable error
    // listing what's available, in both the scoped and multi-query paths
    // (mirrors the MCP server). Without this, the --also path silently returns
    // zero results for a typo'd source.
    if let Some(src) = source {
        let known = store.list_sources()?;
        if !known.iter().any(|s| s.name == src) {
            let names: Vec<&str> = known.iter().map(|s| s.name.as_str()).collect();
            anyhow::bail!("unknown source {src:?}; available: [{}]", names.join(", "));
        }
    }
    output::detail(format!("index: {} (top {top})", store_path.display()));
    let result = if also.is_empty() {
        store.search_scoped(query, top, source)?
    } else {
        let mut queries = vec![query.to_string()];
        queries.extend(also.iter().cloned());
        store.search_multi(&queries, top, source)?
    };
    output::detail(format!(
        "{} matched, {} ranked",
        result.matched_ids.len(),
        result.nodes.len()
    ));

    // Non-JSON formats print a human message and stop; JSON must still emit a
    // well-formed envelope (empty arrays) so programmatic consumers don't choke
    // on zero results.
    if result.nodes.is_empty() && format != QueryFormat::Json {
        output::warn("No results found");
        output::hint("rephrase, or pass --also \"<terms>\" to fuse a reformulation");
        return Ok(());
    }

    // Staleness guard: warn when a returned source's files have
    // changed since indexing, so an agent doesn't trust stale locations.
    let stale = stale_sources_for_result(&store, &result);
    if format != QueryFormat::Json && !stale.is_empty() {
        output::warn(format!(
            "index stale since indexing: {}",
            stale_detail(&stale)
        ));
        output::hint("run `roux add <path>` to refresh");
    }

    match format {
        QueryFormat::Json => {
            let mut value = search_result_to_json(&result);
            if !stale.is_empty()
                && let Some(obj) = value.as_object_mut()
            {
                obj.insert("stale".into(), stale_to_json(&stale));
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        QueryFormat::Skeleton => {
            // Compact, deterministic, prompt-prefix-ready block for use as a
            // one-shot context preprocessor: inject roux's ranked hits
            // into an agent's prompt prefix instead of exposing a live tool.
            // Fields kept minimal on purpose (no edges/scores/bodies); the
            // block is stable run-to-run so it caches cleanly in the prompt prefix.
            print!("{}", render_skeleton(&result));
        }
        QueryFormat::Compact => {
            // Progressive-disclosure block for the live query tool: ranked
            // matched symbols + their neighbor names, under a token budget,
            // with an 'N more' marker. A fraction of the JSON payload.
            print!("{}", render_compact(&result));
        }
        QueryFormat::Text => {
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

/// Char budget for a `--format compact` block. Entries are appended until the
/// next one would exceed this; the rest collapse into an `(… N more)` marker.
/// ~2000 chars ≈ 500 tokens — a live-tool payload small enough to not dominate
/// context, while still carrying the top matches and their graph neighborhood.
const COMPACT_BUDGET_CHARS: usize = 2000;
const COMPACT_SIG_MAX: usize = 160;
const COMPACT_NEAR_MAX: usize = 6;

/// Render a search result as a `--format compact` block for the live query
/// tool and the MCP default: one line per ranked *matched* symbol —
/// `file:line  qualified_name — signature` — with the symbol's graph
/// neighborhood summarized as an indented `near:` line of NAMES (no bodies,
/// ids, scores, or edge arrays). A hard character budget caps the block; matches
/// that don't fit collapse into a trailing `(… N more)` marker. A fraction of
/// the `--format json` payload.
pub fn render_compact(result: &crate::graph::store::SearchResult) -> String {
    render_compact_budgeted(result, COMPACT_BUDGET_CHARS)
}

fn render_compact_budgeted(result: &crate::graph::store::SearchResult, budget: usize) -> String {
    use std::collections::HashSet;

    // Resolve neighbor ids to short names; only neighbors present in the result
    // set are nameable from here (others are trimmed by the search limit).
    let name_of: std::collections::HashMap<&str, &str> = result
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.name.as_str()))
        .collect();
    let matched: HashSet<&str> = result.matched_ids.iter().map(String::as_str).collect();

    let mut out = String::new();
    let mut rendered = 0usize;

    for node in result
        .nodes
        .iter()
        .filter(|n| matched.contains(n.id.as_str()))
    {
        let sig = node
            .signature
            .as_deref()
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.chars().count() > COMPACT_SIG_MAX {
                    format!("{}…", s.chars().take(COMPACT_SIG_MAX).collect::<String>())
                } else {
                    s
                }
            });
        let mut entry = match sig {
            Some(s) => format!(
                "{}:{}  {} — {}\n",
                node.file_path, node.start_line, node.qualified_name, s
            ),
            None => format!(
                "{}:{}  {}\n",
                node.file_path, node.start_line, node.qualified_name
            ),
        };
        let near = compact_neighbor_names(node, result, &name_of);
        if !near.is_empty() {
            entry.push_str(&format!("    near: {}\n", near.join(", ")));
        }

        // Always emit at least one entry, then stop once the budget is hit so
        // the block stays bounded regardless of result size.
        if rendered > 0 && out.len() + entry.len() > budget {
            break;
        }
        out.push_str(&entry);
        rendered += 1;
    }

    let remaining = result.matched_ids.len().saturating_sub(rendered);
    if remaining > 0 {
        out.push_str(&format!(
            "(… {remaining} more match{} — refine the query or request full bodies)\n",
            if remaining == 1 { "" } else { "es" }
        ));
    }
    out
}

/// Names of a node's graph neighbors (edge peers in either direction) that are
/// present in the result set, deduped and capped. Names only — the compact
/// format deliberately omits neighbor bodies and edge kinds.
fn compact_neighbor_names<'a>(
    node: &crate::graph::Node,
    result: &'a crate::graph::store::SearchResult,
    name_of: &std::collections::HashMap<&'a str, &'a str>,
) -> Vec<&'a str> {
    use std::collections::HashSet;
    let mut names = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    seen.insert(node.id.as_str());
    for edge in &result.edges {
        let peer = if edge.from_id == node.id {
            Some(edge.to_id.as_str())
        } else if edge.to_id == node.id {
            Some(edge.from_id.as_str())
        } else {
            None
        };
        if let Some(pid) = peer
            && seen.insert(pid)
            && let Some(name) = name_of.get(pid)
        {
            names.push(*name);
            if names.len() >= COMPACT_NEAR_MAX {
                break;
            }
        }
    }
    names
}

/// Staleness verdict for an indexed source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
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
fn check_source_status(record: &crate::graph::store::SourceRecord) -> Status {
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

/// File-level staleness for the `path` sources present in a query result.
/// Cheap by construction: the fingerprint gate (`check_source_status`,
/// stat-only) skips the per-file walk for unchanged sources, so a fresh local
/// repo costs one stat-walk. Crate/URL sources are immutable at a pinned version
/// and never checked. When the gate trips only because mtimes moved (a
/// checkout/touch with identical content), the authoritative content-hash diff
/// comes back empty and the source is reported fresh.
pub(crate) fn stale_sources_for_result(
    store: &GraphStore,
    result: &crate::graph::store::SearchResult,
) -> Vec<(String, crate::graph::store::FileDiff)> {
    let present: std::collections::BTreeSet<&str> = result
        .nodes
        .iter()
        .map(|n| n.source_name.as_str())
        .collect();
    let Ok(records) = store.list_sources() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for rec in &records {
        if !present.contains(rec.name.as_str())
            || !matches!(check_source_status(rec), Status::Stale(_))
        {
            continue;
        }
        let Some(origin) = rec.origin.as_deref() else {
            continue;
        };
        let root = std::path::Path::new(origin);
        // Walk with the source's index-time language hint (as `add`/`update`
        // do). A `--lang X` source's manifest includes files force-parsed as X
        // (e.g. extensionless configs); a hint-free walk would omit them and
        // report them as spuriously deleted on every real change.
        let hint = (rec.language.as_str() != "unknown").then_some(rec.language.as_str());
        let Ok(current) = crate::graph::extract::list_source_files(root, hint) else {
            continue;
        };
        let Ok(diff) = store.diff_files(&rec.name, &current) else {
            continue;
        };
        if !diff.is_empty() {
            out.push((rec.name.clone(), diff));
        }
    }
    out
}

/// The per-source stale detail (`'name' (N modified, ...)` joined by `; `),
/// shared by the CLI warning and the MCP compact block.
fn stale_detail(stale: &[(String, crate::graph::store::FileDiff)]) -> String {
    let parts: Vec<String> = stale
        .iter()
        .map(|(name, d)| {
            let mut bits = Vec::new();
            if !d.modified.is_empty() {
                bits.push(format!("{} modified", d.modified.len()));
            }
            if !d.added.is_empty() {
                bits.push(format!("{} added", d.added.len()));
            }
            if !d.deleted.is_empty() {
                bits.push(format!("{} deleted", d.deleted.len()));
            }
            format!("'{name}' ({})", bits.join(", "))
        })
        .collect();
    parts.join("; ")
}

/// One-line human warning for stale sources — the MCP compact block. The CLI
/// emits its own `warn`/`hint` pair via [`stale_detail`].
pub(crate) fn format_stale_warning(stale: &[(String, crate::graph::store::FileDiff)]) -> String {
    format!(
        "⚠ index stale since indexing: {} — run `roux add <path>` to refresh",
        stale_detail(stale)
    )
}

/// JSON block describing stale sources, embedded under the query result's
/// `stale` key so agents parsing stdout see it in-band.
pub(crate) fn stale_to_json(
    stale: &[(String, crate::graph::store::FileDiff)],
) -> serde_json::Value {
    serde_json::Value::Array(
        stale
            .iter()
            .map(|(name, d)| {
                serde_json::json!({
                    "source": name,
                    "modified": d.modified,
                    "added": d.added,
                    "deleted": d.deleted,
                })
            })
            .collect(),
    )
}

fn cmd_list(
    config: &Config,
    format: ListFormat,
    local: bool,
    global: bool,
    db: Option<&std::path::Path>,
) -> Result<()> {
    let local_path = std::path::PathBuf::from(".roux/db.sqlite");

    let mut rows: Vec<(crate::graph::store::SourceRecord, Status, String)> = Vec::new();

    if let Some(path) = db {
        let store = open_existing_store(path, true, "")?;
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
        output::warn("No indexed sources");
        output::hint("run `roux init` or `roux add` to build one");
        return Ok(());
    }

    match format {
        ListFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&list_rows_to_json(&rows))?
            );
        }
        ListFormat::Text => {
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

    output::step(format!("roux MCP server: serving {}", store_path.display()));
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

    output::step(format!(
        "Exporting {} → {}",
        source_db.display(),
        output.display()
    ));
    let written = crate::artifact::export(&source_db, output, gzip)?;
    let size = std::fs::metadata(&written).map(|m| m.len()).unwrap_or(0);
    output::done(format!(
        "Wrote {} ({:.1} MiB){}",
        written.display(),
        size as f64 / 1024.0 / 1024.0,
        if gzip { ", gzipped" } else { "" }
    ));
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
    let store = open_existing_store(&store_path, false, ". Run `roux init` or `roux add` first.")?;
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
        output::warn("No sources indexed");
        output::hint("run `roux init` or `roux add` to build one");
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
                output::step(format!("  {:<24} {:<6} fresh", src.name, src.source_kind));
            }
            SyncPlan::Stale { reason, .. } => {
                stale += 1;
                output::step(format!(
                    "  {:<24} {:<6} stale  — {reason}",
                    src.name, src.source_kind
                ));
            }
            SyncPlan::Unknown(why) => {
                unknown += 1;
                output::step(format!(
                    "  {:<24} {:<6} ?      ({why})",
                    src.name, src.source_kind
                ));
            }
        }
    }
    output::step(format!("{fresh} fresh, {stale} stale, {unknown} unknown"));

    if stale == 0 {
        return Ok(());
    }
    if dry_run {
        output::step("dry-run — skipping re-ingest");
        return Ok(());
    }

    // Re-ingest stale sources.
    let timeout = std::time::Duration::from_secs(DEFAULT_CRATE_TIMEOUT_SECS);
    let mut updated = 0;
    let mut failed = 0;
    output::step(format!("Syncing {stale} source(s)"));
    for (src, plan) in &plans {
        let SyncPlan::Stale { action, .. } = plan else {
            continue;
        };
        match action {
            SyncAction::Crate { new_version } => {
                let crate_name = src.origin.as_deref().unwrap_or(&src.name);
                let spin = output::spinner(format!("{crate_name} v{new_version} ..."));
                let outcome = extract_crate_with_timeout(crate_name, new_version, timeout);
                drop(spin);
                match outcome {
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
                            output::warn(format!("{crate_name} v{version} — failed: {e}"));
                            failed += 1;
                        } else {
                            output::ok(format!("{crate_name} v{version} — {count} symbols"));
                            updated += 1;
                        }
                    }
                    CrateOutcome::Err(e) => {
                        output::warn(format!("{crate_name} — failed: {e}"));
                        failed += 1;
                    }
                    CrateOutcome::Timeout => {
                        output::warn(format!("{crate_name} — timeout"));
                        failed += 1;
                    }
                }
            }
            SyncAction::Path => {
                let Some(origin) = src.origin.as_deref() else {
                    output::warn(format!("{} (path) — skip, origin missing", src.name));
                    failed += 1;
                    continue;
                };
                let path = std::path::Path::new(origin);
                match graph::extract::extract_dir(path, &src.name, Some(&src.language)) {
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
                                output::ok(format!(
                                    "{} (path) — {} symbols",
                                    src.name,
                                    fg.nodes.len()
                                ));
                                updated += 1;
                            }
                            Err(e) => {
                                output::warn(format!("{} (path) — failed: {e}", src.name));
                                failed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        output::warn(format!("{} (path) — failed: {e}", src.name));
                        failed += 1;
                    }
                }
            }
            SyncAction::File => {
                let Some(origin) = src.origin.as_deref() else {
                    output::warn(format!("{} (file) — skip, origin missing", src.name));
                    failed += 1;
                    continue;
                };
                let path = std::path::Path::new(origin);
                match graph::extract::extract_file(path, &src.name, Some(&src.language)) {
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
                                output::ok(format!(
                                    "{} (file) — {} symbols",
                                    src.name,
                                    fg.nodes.len()
                                ));
                                updated += 1;
                            }
                            Err(e) => {
                                output::warn(format!("{} (file) — failed: {e}", src.name));
                                failed += 1;
                            }
                        }
                    }
                    Err(e) => {
                        output::warn(format!("{} (file) — failed: {e}", src.name));
                        failed += 1;
                    }
                }
            }
        }
    }

    if failed == 0 {
        output::done(format!("sync complete — {updated} updated"));
    } else {
        output::warn(format!(
            "sync complete — {updated} updated, {failed} failed"
        ));
    }
    Ok(())
}

/// Outcome of refreshing one source.
enum UpdateOutcome {
    /// Nothing changed since the last index.
    UpToDate,
    /// Files changed; `files` present for directory sources, `stats` present
    /// when the delta was actually applied (absent in a dry run).
    Updated {
        files: Option<crate::graph::store::FileDiff>,
        stats: Option<crate::graph::store::DeltaStats>,
    },
}

/// Incrementally refresh path/file sources whose working tree drifted from the
/// index: diff the tree against the stored manifest, re-extract only changed
/// files, and apply the row delta. Crate/URL sources are upstream-versioned, so
/// `roux sync` handles those.
fn cmd_update(
    config: &Config,
    source_filter: Option<&str>,
    local: bool,
    global: bool,
    dry_run: bool,
) -> Result<()> {
    let store_path = config.resolve_store_path(StoreScope::from_flags(local, global));
    let store = open_existing_store(&store_path, false, ". Run `roux init` or `roux add` first.")?;
    let sources = store.list_sources()?;

    let mut considered = 0usize;
    let (mut updated, mut fresh, mut skipped, mut failed) = (0usize, 0usize, 0usize, 0usize);

    for src in &sources {
        if let Some(f) = source_filter
            && src.name != f
        {
            continue;
        }
        // Only locally-editable sources refresh from the working tree.
        if !matches!(src.source_kind.as_str(), "path" | "file") {
            continue;
        }
        considered += 1;

        let Some(origin) = src.origin.as_deref() else {
            output::warn(format!("{} — skipped, origin missing", src.name));
            skipped += 1;
            continue;
        };
        let path = std::path::Path::new(origin);
        if !path.exists() {
            output::warn(format!("{} — skipped, origin gone ({origin})", src.name));
            skipped += 1;
            continue;
        }
        // Reconstruct add's extraction hint: None when detection was unsure, so
        // the incremental parse matches a full rebuild's language handling.
        let hint = (src.language.as_str() != "unknown").then_some(src.language.as_str());

        let outcome = if src.source_kind == "path" {
            update_path_source(&store, src, path, origin, hint, dry_run)
        } else {
            update_file_source(&store, src, path, origin, hint, dry_run)
        };

        match outcome {
            Ok(UpdateOutcome::UpToDate) => {
                output::step(format!("{} — up to date", src.name));
                fresh += 1;
            }
            Ok(UpdateOutcome::Updated { files, stats }) => {
                let filepart = files
                    .map(|f| {
                        format!(
                            " (+{} ~{} -{} files)",
                            f.added.len(),
                            f.modified.len(),
                            f.deleted.len()
                        )
                    })
                    .unwrap_or_default();
                if let Some(s) = stats {
                    output::ok(format!(
                        "{} — updated{filepart}, nodes +{} ~{} -{}, edges +{} -{}",
                        src.name,
                        s.nodes_added,
                        s.nodes_modified,
                        s.nodes_removed,
                        s.edges_added,
                        s.edges_removed
                    ));
                } else {
                    output::step(format!("{} — would update{filepart}", src.name));
                }
                updated += 1;
            }
            Err(e) => {
                output::warn(format!("{} — failed: {e}", src.name));
                failed += 1;
            }
        }
    }

    if considered == 0 {
        if let Some(f) = source_filter {
            anyhow::bail!(
                "no path or file source named '{f}' in {}",
                store_path.display()
            );
        }
        output::warn("No path or file sources to update");
        output::hint("run `roux add <path>` to index a local source first");
        return Ok(());
    }

    let verb = if dry_run { "would update" } else { "updated" };
    if failed == 0 {
        output::done(format!(
            "{updated} {verb}, {fresh} up to date, {skipped} skipped"
        ));
    } else {
        output::warn(format!(
            "{updated} {verb}, {fresh} up to date, {skipped} skipped, {failed} failed"
        ));
    }
    Ok(())
}

/// Refresh a directory source: diff the tree against the manifest and, if
/// anything changed, re-extract only the changed files and apply the delta.
fn update_path_source(
    store: &GraphStore,
    src: &crate::graph::store::SourceRecord,
    path: &std::path::Path,
    origin: &str,
    hint: Option<&str>,
    dry_run: bool,
) -> Result<UpdateOutcome> {
    let current = graph::extract::list_source_files(path, hint)?;
    let files = store.diff_files(&src.name, &current)?;
    if files.added.is_empty() && files.modified.is_empty() && files.deleted.is_empty() {
        // Content is unchanged, but the directory fingerprint (mtime+size) may
        // have drifted from a touch or checkout. Refresh it so staleness checks
        // (`roux status`/`sync`) agree the source is current.
        if !dry_run {
            let fp = crate::fingerprint::fingerprint_dir(path).ok();
            store.set_source_meta(&src.name, "path", Some(origin), fp.as_deref())?;
        }
        return Ok(UpdateOutcome::UpToDate);
    }
    if dry_run {
        return Ok(UpdateOutcome::Updated {
            files: Some(files),
            stats: None,
        });
    }
    let prior = store.source_graph(&src.name)?;
    let new = graph::extract::reextract_incremental(path, &src.name, hint, &prior)?;
    let stats = store.apply_source_delta(
        &src.name,
        &src.version,
        &src.language,
        &new.nodes,
        &new.edges,
    )?;
    store.replace_files(&src.name, &new.files)?;
    let fp = crate::fingerprint::fingerprint_dir(path).ok();
    store.set_source_meta(&src.name, "path", Some(origin), fp.as_deref())?;
    Ok(UpdateOutcome::Updated {
        files: Some(files),
        stats: Some(stats),
    })
}

/// Refresh a single-file source by re-parsing it and applying the delta. A
/// no-op delta (nothing changed) reports up to date.
fn update_file_source(
    store: &GraphStore,
    src: &crate::graph::store::SourceRecord,
    path: &std::path::Path,
    origin: &str,
    hint: Option<&str>,
    dry_run: bool,
) -> Result<UpdateOutcome> {
    if dry_run {
        return Ok(match check_source_status(src) {
            Status::Fresh => UpdateOutcome::UpToDate,
            _ => UpdateOutcome::Updated {
                files: None,
                stats: None,
            },
        });
    }
    let new = graph::extract::extract_file(path, &src.name, hint)?;
    let stats = store.apply_source_delta(
        &src.name,
        &src.version,
        &src.language,
        &new.nodes,
        &new.edges,
    )?;
    if stats == crate::graph::store::DeltaStats::default() {
        return Ok(UpdateOutcome::UpToDate);
    }
    store.replace_files(&src.name, &new.files)?;
    let fp = crate::fingerprint::fingerprint_file(path).ok();
    store.set_source_meta(&src.name, "file", Some(origin), fp.as_deref())?;
    Ok(UpdateOutcome::Updated {
        files: None,
        stats: Some(stats),
    })
}

fn cmd_remove(config: &Config, source_name: &str) -> Result<()> {
    let store_path = config.resolve_store_path(StoreScope::Auto);
    let store = open_existing_store(&store_path, false, "")?;
    store.remove_source(source_name)?;
    output::done(format!("Removed {source_name} from index"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_flag_is_global_before_and_after_subcommand() {
        // A global flag must parse in either position so both `roux --quiet list`
        // and `roux list --quiet` work.
        let before = Cli::try_parse_from(["roux", "--quiet", "list"]).unwrap();
        assert!(before.quiet);
        let after = Cli::try_parse_from(["roux", "list", "--quiet"]).unwrap();
        assert!(after.quiet);
    }

    #[test]
    fn verbose_flag_parses() {
        let cli = Cli::try_parse_from(["roux", "list", "--verbose"]).unwrap();
        assert!(cli.verbose);
        assert!(!cli.quiet);
    }

    #[test]
    fn quiet_and_verbose_conflict() {
        assert!(Cli::try_parse_from(["roux", "--quiet", "--verbose", "list"]).is_err());
    }
    #[test]
    fn staleness_guard_detects_file_changes() {
        use crate::graph::extract;
        let src = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(src.path().join("src")).unwrap();
        std::fs::write(
            src.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\n",
        )
        .unwrap();
        std::fs::write(src.path().join("src/util.rs"), "pub fn helper() {}\n").unwrap();

        let store = GraphStore::open(&dbdir.path().join("index.sqlite")).unwrap();
        let g = extract::extract_dir(src.path(), "demo", Some("rust")).unwrap();
        store
            .upsert_source("demo", "dev", "rust", &g.nodes, &g.edges)
            .unwrap();
        store.replace_files("demo", &g.files).unwrap();
        let fp = crate::fingerprint::fingerprint_dir(src.path()).ok();
        store
            .set_source_meta("demo", "path", src.path().to_str(), fp.as_deref())
            .unwrap();

        let result = store.search("alpha", 5).unwrap();
        assert!(!result.nodes.is_empty());

        // Fresh index → guard stays silent.
        assert!(stale_sources_for_result(&store, &result).is_empty());

        // Modify one file, delete one, add one.
        std::fs::write(
            src.path().join("src/lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
        )
        .unwrap();
        std::fs::remove_file(src.path().join("src/util.rs")).unwrap();
        std::fs::write(src.path().join("src/new.rs"), "pub fn brandnew() {}\n").unwrap();

        let stale = stale_sources_for_result(&store, &result);
        assert_eq!(stale.len(), 1);
        let (name, diff) = &stale[0];
        assert_eq!(name, "demo");
        assert_eq!(diff.modified, vec!["src/lib.rs"]);
        assert_eq!(diff.deleted, vec!["src/util.rs"]);
        assert_eq!(diff.added, vec!["src/new.rs"]);
    }

    #[test]
    fn stale_check_honors_index_time_lang_hint_no_spurious_deletes() {
        use crate::graph::extract;
        let src = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("lib.rs"), "pub fn findable_symbol() {}\n").unwrap();
        // Non-grammar file: a rust hint force-parses it into the manifest at
        // index time, but a hint-free walk (the old buggy behavior) would
        // never see it and would report it as deleted.
        std::fs::write(src.path().join("config.txt"), "some = setting\n").unwrap();

        let store = GraphStore::open(&dbdir.path().join("index.sqlite")).unwrap();
        let fg = extract::extract_dir(src.path(), "mixed", Some("rust")).unwrap();
        assert!(
            fg.files.iter().any(|f| f.path == "config.txt"),
            "sanity: rust hint must force-parse config.txt into the manifest"
        );
        store
            .upsert_source("mixed", "0", "rust", &fg.nodes, &fg.edges)
            .unwrap();
        store.replace_files("mixed", &fg.files).unwrap();
        // Deliberately wrong fingerprint trips the staleness gate without
        // touching any file on disk.
        store
            .set_source_meta(
                "mixed",
                "path",
                src.path().to_str(),
                Some("deadbeefdeadbeef"),
            )
            .unwrap();

        let result = store.search("findable_symbol", 10).unwrap();
        assert!(result.nodes.iter().any(|n| n.source_name == "mixed"));

        let stale = stale_sources_for_result(&store, &result);
        assert!(
            stale.is_empty(),
            "unchanged content must not be reported stale: {stale:?}"
        );
    }

    #[test]
    fn test_parse_add() {
        let cli = Cli::try_parse_from(["roux", "add", "tokio"]).unwrap();
        assert!(
            matches!(cli.command, Command::Add { ref sources, .. } if sources.len() == 1 && sources[0] == "tokio")
        );
    }

    #[test]
    fn test_parse_add_multiple_sources() {
        let cli = Cli::try_parse_from(["roux", "add", "tokio", "serde", "futures"]).unwrap();
        assert!(matches!(cli.command, Command::Add { ref sources, .. } if sources.len() == 3));
    }

    #[test]
    fn test_add_requires_a_source() {
        assert!(Cli::try_parse_from(["roux", "add"]).is_err());
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

    // ─── compact format ────────────────────────────────────────────────
    use crate::graph::store::SearchResult;
    use crate::graph::{Edge, Node};

    fn node(name: &str, sig: Option<&str>, doc: Option<&str>) -> Node {
        Node {
            id: name.to_string(), // use the name as id for readable test edges
            kind: "function".into(),
            name: name.into(),
            qualified_name: format!("demo::{name}"),
            source_name: "demo".into(),
            language: "rust".into(),
            file_path: "lib.rs".into(),
            start_line: 10,
            start_col: 0,
            end_line: 20,
            visibility: "pub".into(),
            signature: sig.map(str::to_string),
            doc: doc.map(str::to_string),
            body: String::new(),
            parent_id: None,
            content_hash: None,
            line_count: 10,
            source_url: None,
            description: None,
        }
    }

    #[test]
    fn compact_renders_matched_with_neighbor_names() {
        let result = SearchResult {
            matched_ids: vec!["build".into()],
            nodes: vec![
                node(
                    "build",
                    Some("pub fn build() -> Searcher"),
                    Some("Build a searcher."),
                ),
                node("Searcher", None, None), // neighbor, not matched
            ],
            edges: vec![Edge {
                from_id: "build".into(),
                to_id: "Searcher".into(),
                kind: "type_ref".into(),
                ref_name: None,
            }],
            scores: Default::default(),
        };
        let out = render_compact(&result);
        assert!(
            out.contains("lib.rs:10  demo::build — pub fn build() -> Searcher"),
            "got:\n{out}"
        );
        // neighbor appears as a NAME on the near: line, not its own primary entry
        assert!(out.contains("near: Searcher"), "got:\n{out}");
        assert!(
            !out.contains("lib.rs:10  demo::Searcher"),
            "neighbor should not be a primary entry:\n{out}"
        );
    }

    #[test]
    fn compact_is_a_fraction_of_json() {
        let result = SearchResult {
            matched_ids: vec!["build".into()],
            nodes: vec![
                node(
                    "build",
                    Some("pub fn build() -> Searcher"),
                    Some("Build a searcher."),
                ),
                node(
                    "Searcher",
                    Some("pub struct Searcher"),
                    Some("The searcher."),
                ),
            ],
            edges: vec![Edge {
                from_id: "build".into(),
                to_id: "Searcher".into(),
                kind: "type_ref".into(),
                ref_name: None,
            }],
            scores: Default::default(),
        };
        let compact = render_compact(&result);
        let json = serde_json::to_string_pretty(&search_result_to_json(&result)).unwrap();
        assert!(
            compact.len() * 2 < json.len(),
            "compact ({}) should be far smaller than json ({})",
            compact.len(),
            json.len()
        );
    }

    #[test]
    fn compact_budget_truncates_with_more_marker() {
        // Two matches, budget too small for both → one entry + "1 more" marker.
        let result = SearchResult {
            matched_ids: vec!["alpha".into(), "beta".into()],
            nodes: vec![
                node("alpha", Some("pub fn alpha()"), Some("First.")),
                node("beta", Some("pub fn beta()"), Some("Second.")),
            ],
            edges: vec![],
            scores: Default::default(),
        };
        let out = render_compact_budgeted(&result, 40);
        assert!(out.contains("demo::alpha"), "got:\n{out}");
        assert!(
            !out.contains("demo::beta"),
            "beta should be budgeted out:\n{out}"
        );
        assert!(out.contains("1 more match "), "got:\n{out}");
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
        // --local and --global are mutually exclusive on query, list, and init.
        assert!(Cli::try_parse_from(["roux", "query", "x", "--local", "--global"]).is_err());
        assert!(Cli::try_parse_from(["roux", "list", "--local", "--global"]).is_err());
        assert!(Cli::try_parse_from(["roux", "init", "--local", "--global"]).is_err());
    }

    #[test]
    fn test_parse_format_is_a_validated_enum() {
        // Valid formats parse; an unknown value is a hard error, not a silent
        // fall-through to text.
        for f in ["text", "json", "skeleton", "compact"] {
            Cli::try_parse_from(["roux", "query", "x", "--format", f]).unwrap();
        }
        for f in ["text", "json"] {
            Cli::try_parse_from(["roux", "list", "--format", f]).unwrap();
        }
        assert!(Cli::try_parse_from(["roux", "query", "x", "--format", "josn"]).is_err());
        assert!(Cli::try_parse_from(["roux", "list", "--format", "yaml"]).is_err());
        // list must reject query-only formats.
        assert!(Cli::try_parse_from(["roux", "list", "--format", "skeleton"]).is_err());
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

    // --- index_project on manifest-less / monorepo / mixed trees ---

    fn index_temp(dir: &Path) -> GraphStore {
        let store = GraphStore::open_in_memory().unwrap();
        index_project(dir, &store, false, &[], Duration::from_secs(1)).unwrap();
        store
    }

    #[test]
    fn init_indexes_manifestless_cpp_repo() {
        // Marlin-shape: C++ firmware, no lockfile/manifest. Must index anyway.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("motion.cpp"),
            "void plan_buffer_line() { int steps = 0; }\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("temperature.h"), "void manage_heater();\n").unwrap();

        let store = index_temp(tmp.path());
        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1, "local source should be indexed");
        assert!(sources[0].node_count > 0, "expected symbols from cpp files");
        assert_eq!(
            sources[0].language, "cpp",
            "dominant language should be cpp"
        );
        assert!(
            !store
                .search("plan_buffer_line", 5)
                .unwrap()
                .matched_ids
                .is_empty(),
            "a known cpp symbol should be searchable"
        );
    }

    #[test]
    fn init_indexes_monorepo_with_no_root_deps() {
        // remix-shape: manifest present but no root deps; source lives in packages/*.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{\"name\":\"root\"}\n").unwrap();
        let pkg = tmp.path().join("packages").join("app");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(
            pkg.join("index.js"),
            "function handleRequest() { return 1; }\n",
        )
        .unwrap();

        let store = index_temp(tmp.path());
        assert!(
            !store
                .search("handleRequest", 5)
                .unwrap()
                .matched_ids
                .is_empty(),
            "monorepo package source should be indexed even with no root deps"
        );
    }

    #[test]
    fn init_detects_language_per_file_in_mixed_tree() {
        // The old single-language hint parsed every file as the project language;
        // per-file detection must parse each by its own extension.
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "pub fn rusty_fn() {}\n").unwrap();
        std::fs::write(
            tmp.path().join("script.py"),
            "def pythonic_fn():\n    pass\n",
        )
        .unwrap();

        let store = index_temp(tmp.path());
        // If either file were parsed as the other's language, its symbol would
        // not extract — so both hits prove per-file detection.
        let rust_hit = !store.search("rusty_fn", 5).unwrap().matched_ids.is_empty();
        let py_hit = !store
            .search("pythonic_fn", 5)
            .unwrap()
            .matched_ids
            .is_empty();
        assert!(
            rust_hit && py_hit,
            "both rust and python symbols should index"
        );
    }
}
