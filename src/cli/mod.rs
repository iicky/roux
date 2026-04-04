use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::embed::Embedder;
use crate::embed::candle::{CandleEmbedder, PrefixStyle};
use crate::extract;
use crate::extract::RawChunk;
use crate::graph;
use crate::graph::store::GraphStore;
use crate::model;
use crate::source::Source;
use crate::source::SourceKind;
use crate::store::sqlite::SqliteStore;
use crate::store::{Chunk, Store};

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
        /// Also compute embeddings (slower, enables semantic search)
        #[arg(long)]
        embed: bool,
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
    /// Manage the local embedding model
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// Download the default embedding model
    Download,
    /// Show loaded model info
    Status,
    /// Switch to a different model
    Set {
        /// Model identifier
        model_id: String,
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
            Command::Init { .. } => todo!("init"),
            Command::Add {
                source,
                lang,
                local,
                version,
                name,
                embed,
            } => cmd_add(
                &config,
                source,
                name.clone(),
                lang.clone(),
                version.clone(),
                *local,
                *embed,
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
            Command::List { format } => cmd_list(&config, format),
            Command::Sync { .. } => todo!("sync"),
            Command::Remove { source } => cmd_remove(&config, source),
            Command::Model { action } => match action {
                ModelAction::Download => cmd_model_download(&config),
                ModelAction::Status => cmd_model_status(&config),
                ModelAction::Set { .. } => todo!("model set"),
            },
        }
    }
}

fn cmd_add(
    config: &Config,
    raw_source: &str,
    name: Option<String>,
    lang: Option<String>,
    version: Option<String>,
    local: bool,
    _embed: bool,
) -> Result<()> {
    let source = Source::from_raw(raw_source, name, lang, version);
    let source_version = source.version.as_deref().unwrap_or("unknown");
    let language = source.detected_language().unwrap_or("unknown").to_string();

    eprintln!("Extracting graph from {}...", source.name);

    // Use tree-sitter graph extraction
    let file_graph = match &source.kind {
        SourceKind::LocalPath(path) => {
            graph::extract::extract_dir(path, &source.name, source_version, Some(&language))?
        }
        SourceKind::File(path) => {
            graph::extract::extract_file(path, &source.name, source_version, Some(&language))?
        }
        SourceKind::Crate(crate_name) => {
            // Download crate, then extract
            let version_str = source.version.as_deref().unwrap_or("latest");
            let dir = crate::extract::rustdoc::download_crate(crate_name, version_str)?;
            graph::extract::extract_dir(&dir, &source.name, source_version, Some("rust"))?
        }
        SourceKind::Url(_) => anyhow::bail!("URL sources not yet supported for graph extraction"),
    };

    eprintln!(
        "Extracted {} symbols, {} edges",
        file_graph.symbols.len(),
        file_graph.edges.len()
    );

    if file_graph.symbols.is_empty() {
        eprintln!("No symbols found.");
        return Ok(());
    }

    // Store in graph database
    let store_path = config.resolve_store_path(local);
    let store = GraphStore::open(&store_path)?;
    store.upsert_source(
        &source.name,
        source_version,
        &language,
        &file_graph.symbols,
        &file_graph.edges,
    )?;

    eprintln!(
        "Indexed {} symbols from {} into {}",
        file_graph.symbols.len(),
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

    if result.symbols.is_empty() {
        eprintln!("No results found.");
        return Ok(());
    }

    match format {
        "json" => {
            let json_result = serde_json::json!({
                "matched": result.matched_ids,
                "symbols": result.symbols.iter().map(|s| {
                    serde_json::json!({
                        "id": s.id,
                        "kind": s.kind,
                        "name": s.name,
                        "qualified_name": s.qualified_name,
                        "file": s.file_path,
                        "line": s.line,
                        "signature": s.signature,
                        "doc": s.doc,
                        "parent_id": s.parent_id,
                        "matched": result.matched_ids.contains(&s.id),
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
            for sym in &result.symbols {
                let is_match = result.matched_ids.contains(&sym.id);
                let marker = if is_match { "●" } else { "○" };
                let kind_str = &sym.kind;

                println!(
                    "{marker} {} ({kind_str}) {}:{}",
                    sym.qualified_name, sym.file_path, sym.line
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
                    if let Some(target) = result.symbols.iter().find(|s| s.id == edge.to_id) {
                        println!("  → {} {} ({})", edge.kind, target.name, target.kind);
                    }
                }
                println!();
            }
        }
    }

    Ok(())
}

fn cmd_list(config: &Config, format: &str) -> Result<()> {
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
                        "symbols": s.symbol_count,
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
                    src.name, src.version, src.language, src.symbol_count
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

fn cmd_model_download(config: &Config) -> Result<()> {
    eprintln!("Downloading model {}...", config.model.id);
    let files = model::ensure_model(&config.model.id)?;
    eprintln!("Model ready at {}", files.model.display());
    Ok(())
}

fn cmd_model_status(config: &Config) -> Result<()> {
    let status = model::status(&config.model.id)?;
    println!("{status}");
    Ok(())
}

/// Split oversized chunks into sub-chunks that fit within the model's token limit.
fn split_oversized_chunks(raw_chunks: Vec<RawChunk>, embedder: &CandleEmbedder) -> Vec<RawChunk> {
    let max_tokens = embedder.max_tokens();
    // Reserve tokens for the "passage: " prefix the embedder adds
    let effective_limit = max_tokens.saturating_sub(10);
    let mut result = Vec::with_capacity(raw_chunks.len());

    for chunk in raw_chunks {
        let token_count = embedder.token_count(&chunk.body);
        if token_count <= effective_limit {
            result.push(chunk);
            continue;
        }

        // Split on paragraph boundaries first, then sentence boundaries
        let parts =
            split_text_to_token_limit(&chunk.body, effective_limit, |t| embedder.token_count(t));

        for (i, part) in parts.into_iter().enumerate() {
            let mut sub = chunk.clone();
            sub.qualified_name = format!("{} [part {}]", chunk.qualified_name, i + 1);
            sub.body = part.clone();
            sub.doc = part;
            result.push(sub);
        }
    }

    result
}

/// Split text into parts that each fit within a token limit.
/// Tries paragraph boundaries first, then sentence boundaries, then hard split.
fn split_text_to_token_limit(
    text: &str,
    limit: usize,
    count_tokens: impl Fn(&str) -> usize,
) -> Vec<String> {
    // Try splitting on double newlines (paragraphs)
    let paragraphs: Vec<&str> = text.split("\n\n").collect();
    let mut parts = Vec::new();
    let mut current = String::new();

    for para in paragraphs {
        let candidate = if current.is_empty() {
            para.to_string()
        } else {
            format!("{current}\n\n{para}")
        };

        if count_tokens(&candidate) <= limit {
            current = candidate;
        } else if current.is_empty() {
            // Single paragraph exceeds limit — split on sentences
            let sentences = split_sentences(para);
            let mut sent_buf = String::new();
            for sent in sentences {
                let sent_candidate = if sent_buf.is_empty() {
                    sent.to_string()
                } else {
                    format!("{sent_buf} {sent}")
                };
                if count_tokens(&sent_candidate) <= limit {
                    sent_buf = sent_candidate;
                } else {
                    if !sent_buf.is_empty() {
                        parts.push(sent_buf);
                    }
                    sent_buf = sent.to_string();
                }
            }
            if !sent_buf.is_empty() {
                current = sent_buf;
            }
        } else {
            parts.push(current);
            current = para.to_string();
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }

    parts
}

fn split_sentences(text: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0;
    for (i, _) in text.match_indices(['.', '!', '?']) {
        let end = i + 1;
        let s = text[start..end].trim();
        if !s.is_empty() {
            sentences.push(s);
        }
        start = end;
    }
    let remaining = text[start..].trim();
    if !remaining.is_empty() {
        sentences.push(remaining);
    }
    sentences
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
    fn test_parse_model_download() {
        Cli::try_parse_from(["roux", "model", "download"]).unwrap();
    }

    #[test]
    fn test_parse_model_status() {
        Cli::try_parse_from(["roux", "model", "status"]).unwrap();
    }

    #[test]
    fn test_parse_model_set() {
        Cli::try_parse_from(["roux", "model", "set", "BAAI/bge-small-en-v1.5"]).unwrap();
    }

    #[test]
    fn test_parse_no_args_fails() {
        assert!(Cli::try_parse_from(["roux"]).is_err());
    }

    #[test]
    fn test_parse_unknown_command_fails() {
        assert!(Cli::try_parse_from(["roux", "unknown"]).is_err());
    }

    #[test]
    fn test_split_text_to_token_limit() {
        // Simple token counter: 1 token per word
        let count = |t: &str| t.split_whitespace().count();

        let text = "Hello world. This is a test.\n\nSecond paragraph here.";
        let parts = split_text_to_token_limit(text, 10, count);
        assert!(!parts.is_empty());
        for part in &parts {
            assert!(count(part) <= 10, "part too long: {part}");
        }
    }

    #[test]
    fn test_split_text_short_text_not_split() {
        let count = |t: &str| t.split_whitespace().count();
        let text = "Short text.";
        let parts = split_text_to_token_limit(text, 100, count);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], "Short text.");
    }

    #[test]
    fn test_split_text_paragraph_boundaries() {
        let count = |t: &str| t.split_whitespace().count();
        let text = "First paragraph with some words.\n\nSecond paragraph with more words.\n\nThird paragraph here.";
        let parts = split_text_to_token_limit(text, 6, count);
        assert!(parts.len() >= 2);
        for part in &parts {
            assert!(count(part) <= 6, "part too long: {part}");
        }
    }

    #[test]
    fn test_split_sentences() {
        let sents = split_sentences("Hello world. How are you? Fine!");
        assert_eq!(sents.len(), 3);
        assert_eq!(sents[0], "Hello world.");
        assert_eq!(sents[1], "How are you?");
        assert_eq!(sents[2], "Fine!");
    }
}
