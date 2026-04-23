use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::config::Config;
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
        /// Number of results
        #[arg(long, default_value = "3")]
        top: usize,
        /// Restrict search to a named source
        #[arg(long)]
        source: Option<String>,
        /// Output format: text or json
        #[arg(long, default_value = "text")]
        format: String,
        /// Search local index only
        #[arg(long)]
        local: bool,
        /// Search global index only
        #[arg(long)]
        global: bool,
    },
    /// List all indexed sources
    List {
        /// Output format: text or json
        #[arg(long, default_value = "text")]
        format: String,
        /// List local index only
        #[arg(long)]
        local: bool,
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
                top,
                source,
                format,
                local,
                global,
            } => cmd_query(
                &config,
                query,
                *top,
                source.as_deref(),
                format,
                *local,
                *global,
            ),
            Command::List { format, local } => cmd_list(&config, format, *local),
            Command::Sync { .. } => todo!("sync"),
            Command::Remove { source } => cmd_remove(&config, source),
        }
    }
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

    let store_path = config.resolve_store_path(local);
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
                    match store.upsert_source(
                        &dep.name,
                        &version,
                        "rust",
                        &graph.nodes,
                        &graph.edges,
                    ) {
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
    let file_graph = match &source.kind {
        SourceKind::LocalPath(path) => {
            graph::extract::extract_dir(path, &source.name, &source_version, Some(&language))?
        }
        SourceKind::File(path) => {
            graph::extract::extract_file(path, &source.name, &source_version, Some(&language))?
        }
        SourceKind::Crate(crate_name) => {
            let version_str = source.version.as_deref().unwrap_or("latest");
            let (dir, resolved_version) =
                crate::source::crate_download::download_crate(crate_name, version_str)?;
            source_version = resolved_version;
            graph::extract::extract_dir(&dir, &source.name, &source_version, Some("rust"))?
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
    let store_path = config.resolve_store_path(local);
    let store = GraphStore::open(&store_path)?;
    store.upsert_source(
        &source.name,
        &source_version,
        &language,
        &file_graph.nodes,
        &file_graph.edges,
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
    _config: &Config,
    query: &str,
    top: usize,
    _source: Option<&str>,
    format: &str,
    local: bool,
    _global: bool,
) -> Result<()> {
    let store_path = if local {
        std::path::PathBuf::from(".roux/db.sqlite")
    } else {
        let config = Config::load()?;
        config.resolve_store_path(false)
    };

    if !store_path.exists() {
        anyhow::bail!("no index found at {}", store_path.display());
    }

    let store = GraphStore::open(&store_path)?;
    let result = store.search(query, top)?;

    if result.nodes.is_empty() {
        eprintln!("No results found.");
        return Ok(());
    }

    match format {
        "json" => {
            let json_result = serde_json::json!({
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
            });
            println!("{}", serde_json::to_string_pretty(&json_result)?);
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

fn cmd_list(config: &Config, format: &str, local: bool) -> Result<()> {
    let _ = local; // TODO: use to filter local vs global
    let store_path = config.resolve_store_path(false);
    let local_path = std::path::PathBuf::from(".roux/db.sqlite");

    let mut all_sources = Vec::new();

    if local_path.exists() {
        let store = GraphStore::open(&local_path)?;
        for mut src in store.list_sources()? {
            src.name = format!("{} (local)", src.name);
            all_sources.push(src);
        }
    }

    if store_path.exists() && store_path != local_path {
        let store = GraphStore::open(&store_path)?;
        all_sources.extend(store.list_sources()?);
    }

    if all_sources.is_empty() {
        eprintln!("No indexed sources.");
        return Ok(());
    }

    match format {
        "json" => {
            let json: Vec<serde_json::Value> = all_sources
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "version": s.version,
                        "language": s.language,
                        "symbols": s.node_count,
                        "ingested_at": s.ingested_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        _ => {
            println!(
                "{:<20} {:<12} {:<10} {:>8}",
                "SOURCE", "VERSION", "LANGUAGE", "SYMBOLS"
            );
            println!("{}", "─".repeat(54));
            for src in &all_sources {
                println!(
                    "{:<20} {:<12} {:<10} {:>8}",
                    src.name, src.version, src.language, src.node_count
                );
            }
        }
    }

    Ok(())
}

fn cmd_remove(config: &Config, source_name: &str) -> Result<()> {
    let store_path = config.resolve_store_path(false);
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
            "roux", "init", "--exclude", "candle*", "--exclude", "*-sys", "--timeout", "15",
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

    #[test]
    fn test_parse_list() {
        Cli::try_parse_from(["roux", "list"]).unwrap();
        Cli::try_parse_from(["roux", "list", "--format", "json"]).unwrap();
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
    fn test_parse_no_args_fails() {
        assert!(Cli::try_parse_from(["roux"]).is_err());
    }

    #[test]
    fn test_parse_unknown_command_fails() {
        assert!(Cli::try_parse_from(["roux", "unknown"]).is_err());
    }
}
