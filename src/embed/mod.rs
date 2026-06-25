pub mod candle;

use std::path::PathBuf;

use anyhow::{Context, Result};

pub trait Embedder {
    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
}

/// Default embedding model: a code-specialized Jina v2 (ALiBi BERT, 768-dim).
pub const DEFAULT_MODEL_ID: &str = "jinaai/jina-embeddings-v2-base-code";

/// The model id to use, overridable via `ROUX_EMBED_MODEL` so models can be
/// A/B-tested without recompiling. Both ingest and query must agree, so callers
/// should route through this.
pub fn model_id() -> String {
    std::env::var("ROUX_EMBED_MODEL").unwrap_or_else(|_| DEFAULT_MODEL_ID.to_string())
}

const MODEL_FILES: &[&str] = &["model.safetensors", "tokenizer.json", "config.json"];

/// Local cache paths for a model's files.
pub struct ModelFiles {
    pub model: PathBuf,
    pub tokenizer: PathBuf,
    pub config: PathBuf,
}

/// Resolve a model's files from the local HuggingFace cache, downloading any
/// that are missing. Network is only touched the first time (or on cache miss).
pub fn ensure_model(model_id: &str) -> Result<ModelFiles> {
    let api = hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
    let repo = api.model(model_id.to_string());

    let mut paths = Vec::with_capacity(MODEL_FILES.len());
    for filename in MODEL_FILES {
        let path = repo
            .get(filename)
            .with_context(|| format!("failed to fetch {filename} from {model_id}"))?;
        paths.push(path);
    }

    Ok(ModelFiles {
        model: paths[0].clone(),
        tokenizer: paths[1].clone(),
        config: paths[2].clone(),
    })
}
