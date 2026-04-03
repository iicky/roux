use std::path::PathBuf;

use anyhow::{Context, Result};

pub const DEFAULT_MODEL_ID: &str = "intfloat/multilingual-e5-small";

const MODEL_FILES: &[&str] = &["model.safetensors", "tokenizer.json", "config.json"];

/// Returns the local cache path for a model file, downloading it if necessary.
/// Known safe model IDs. Warn if user configures an unknown model.
const KNOWN_MODELS: &[&str] = &[
    "intfloat/multilingual-e5-small",
    "intfloat/e5-small-v2",
    "BAAI/bge-small-en-v1.5",
    "jinaai/jina-embeddings-v2-base-code",
    "nomic-ai/nomic-embed-code",
];

pub fn ensure_model(model_id: &str) -> Result<ModelFiles> {
    if !KNOWN_MODELS.contains(&model_id) {
        eprintln!(
            "warning: model {model_id:?} is not in the known models list — verify you trust this source"
        );
    }
    let api = hf_hub::api::sync::Api::new().context("failed to create HuggingFace API client")?;
    let repo = api.model(model_id.to_string());

    let mut paths = Vec::new();
    for filename in MODEL_FILES {
        eprintln!("Checking {filename}...");
        let path = repo
            .get(filename)
            .with_context(|| format!("failed to download {filename} from {model_id}"))?;
        paths.push(path);
    }

    Ok(ModelFiles {
        model: paths[0].clone(),
        tokenizer: paths[1].clone(),
        config: paths[2].clone(),
    })
}

pub struct ModelFiles {
    pub model: PathBuf,
    pub tokenizer: PathBuf,
    pub config: PathBuf,
}

/// Check if a model is available locally.
pub fn status(model_id: &str) -> Result<String> {
    let api = hf_hub::api::sync::Api::new()?;
    let repo = api.model(model_id.to_string());

    let mut available = Vec::new();
    let mut missing = Vec::new();

    for filename in MODEL_FILES {
        // Check if file is in local cache by trying to resolve it
        match repo.get(filename) {
            Ok(_) => available.push(*filename),
            Err(_) => missing.push(*filename),
        }
    }

    if missing.is_empty() {
        Ok(format!("{model_id}: all files available"))
    } else {
        Ok(format!("{model_id}: missing {}", missing.join(", ")))
    }
}
