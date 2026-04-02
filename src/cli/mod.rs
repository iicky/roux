use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::embed::Embedder;
use crate::embed::candle::CandleEmbedder;
use crate::extract;
use crate::extract::RawChunk;
use crate::model;
use crate::source::Source;
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
) -> Result<()> {
    let source = Source::from_raw(raw_source, name, lang, version);
    eprintln!("Extracting chunks from {}...", source.name);

    // Extract
    let raw_chunks = extract::extract(&source)?;
    eprintln!("Extracted {} chunks", raw_chunks.len());

    if raw_chunks.is_empty() {
        eprintln!("No documented items found.");
        return Ok(());
    }

    // Embed
    eprintln!("Loading embedding model...");
    let model_files = model::ensure_model(&config.model.id)?;
    let embedder = CandleEmbedder::load(
        &model_files.model,
        &model_files.tokenizer,
        &model_files.config,
    )?;

    eprintln!("Embedding {} chunks...", raw_chunks.len());
    let chunks = embed_raw_chunks(&raw_chunks, &embedder, config.ingest.batch_size)?;

    // Store
    let store_path = config.resolve_store_path(local);
    let store = SqliteStore::open(&store_path)?;
    store.upsert_chunks(&chunks)?;

    eprintln!(
        "Indexed {} chunks from {} into {}",
        chunks.len(),
        source.name,
        store_path.display()
    );
    Ok(())
}

fn cmd_query(
    config: &Config,
    query: &str,
    top: usize,
    source: Option<&str>,
    format: &str,
    local: bool,
    _global: bool,
) -> Result<()> {
    // Load model
    let model_files = model::ensure_model(&config.model.id)?;
    let embedder = CandleEmbedder::load(
        &model_files.model,
        &model_files.tokenizer,
        &model_files.config,
    )?;

    // Embed query
    let query_embedding = embedder.embed_query(query)?;

    // Search
    let store_path = config.resolve_store_path(local);
    let store = SqliteStore::open(&store_path)?;
    let results = store.search(&query_embedding, top, source)?;

    if results.is_empty() {
        eprintln!("No results found.");
        return Ok(());
    }

    match format {
        "json" => {
            let json_results: Vec<serde_json::Value> = results
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "qualified_name": c.qualified_name,
                        "item_type": c.item_type,
                        "source_name": c.source_name,
                        "source_version": c.source_version,
                        "signature": c.signature,
                        "doc": c.doc,
                        "url": c.url,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json_results)?);
        }
        _ => {
            for (i, chunk) in results.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("── {} ({}) ──", chunk.qualified_name, chunk.item_type);
                println!("source: {}@{}", chunk.source_name, chunk.source_version);
                if let Some(sig) = &chunk.signature {
                    println!("{sig}");
                }
                println!();
                println!("{}", chunk.doc);
            }
        }
    }

    Ok(())
}

fn cmd_list(config: &Config, format: &str) -> Result<()> {
    // Check both local and global
    let mut all_sources = Vec::new();

    let local_path = std::path::PathBuf::from(".roux/db.sqlite");
    if local_path.exists() {
        let store = SqliteStore::open(&local_path)?;
        for mut src in store.list_sources()? {
            src.name = format!("{} (local)", src.name);
            all_sources.push(src);
        }
    }

    let global_path = &config.index.global_path;
    if global_path.exists() {
        let store = SqliteStore::open(global_path)?;
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
                        "chunks": s.chunk_count,
                        "ingested_at": s.ingested_at,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&json)?);
        }
        _ => {
            println!(
                "{:<20} {:<12} {:<10} {:>6}",
                "SOURCE", "VERSION", "LANGUAGE", "CHUNKS"
            );
            println!("{}", "─".repeat(52));
            for src in &all_sources {
                println!(
                    "{:<20} {:<12} {:<10} {:>6}",
                    src.name, src.version, src.language, src.chunk_count
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

    let store = SqliteStore::open(&store_path)?;
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

/// Convert RawChunks to Chunks by embedding them in batches.
fn embed_raw_chunks(
    raw_chunks: &[RawChunk],
    embedder: &CandleEmbedder,
    batch_size: usize,
) -> Result<Vec<Chunk>> {
    let mut chunks = Vec::with_capacity(raw_chunks.len());

    for batch in raw_chunks.chunks(batch_size) {
        let texts: Vec<&str> = batch.iter().map(|c| c.body.as_str()).collect();
        let embeddings = embedder
            .embed_passages(&texts)
            .context("embedding batch failed")?;

        for (raw, embedding) in batch.iter().zip(embeddings) {
            chunks.push(Chunk {
                id: raw.id(),
                source_name: raw.source_name.clone(),
                source_version: raw.source_version.clone(),
                language: raw.language.clone(),
                item_type: raw.item_type.clone(),
                qualified_name: raw.qualified_name.clone(),
                signature: raw.signature.clone(),
                doc: raw.doc.clone(),
                body: raw.body.clone(),
                embedding,
                url: raw.url.clone(),
                ingested_at: 0,
            });
        }
    }

    Ok(chunks)
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
}
