use std::path::Path;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::Tokenizer;

use super::Embedder;

/// Select the best available device: Metal > CUDA > CPU.
fn best_device() -> Result<Device> {
    #[cfg(feature = "metal")]
    {
        if let Ok(device) = Device::new_metal(0) {
            return Ok(device);
        }
    }

    #[cfg(feature = "cuda")]
    {
        if let Ok(device) = Device::new_cuda(0) {
            return Ok(device);
        }
    }

    Ok(Device::Cpu)
}

pub struct CandleEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
    embedding_dim: usize,
}

impl CandleEmbedder {
    /// Load a sentence-transformer model from a local directory.
    /// Expects: model.safetensors, tokenizer.json, config.json
    pub fn load(model_dir: &Path) -> Result<Self> {
        let device = best_device()?;

        let config_path = model_dir.join("config.json");
        let model_path = model_dir.join("model.safetensors");
        let tokenizer_path = model_dir.join("tokenizer.json");

        let config_str =
            std::fs::read_to_string(&config_path).context("reading model config.json")?;
        let config: BertConfig =
            serde_json::from_str(&config_str).context("parsing model config.json")?;

        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[model_path], DType::F32, &device)
                .context("loading model weights")?
        };
        let model = BertModel::load(vb, &config).context("building BERT model")?;

        let mut tokenizer =
            Tokenizer::from_file(&tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let truncation = tokenizers::TruncationParams {
            max_length: 512,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        let embedding_dim = config.hidden_size;

        Ok(Self {
            model,
            tokenizer,
            device,
            embedding_dim,
        })
    }

    /// Download a model from HuggingFace Hub if not already cached.
    pub fn from_pretrained(model_id: &str) -> Result<Self> {
        let api = hf_hub::api::sync::Api::new()?;
        let repo = api.model(model_id.to_string());

        // Download required files
        let model_path = repo.get("model.safetensors")?;
        let model_dir = model_path
            .parent()
            .context("model path has no parent directory")?;

        // Ensure tokenizer and config are also downloaded
        repo.get("tokenizer.json")?;
        repo.get("config.json")?;

        Self::load(model_dir)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        let mut all_ids = Vec::with_capacity(encodings.len() * max_len);
        let mut all_type_ids = Vec::with_capacity(encodings.len() * max_len);
        let mut all_mask = Vec::with_capacity(encodings.len() * max_len);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let type_ids = encoding.get_type_ids();
            let mask = encoding.get_attention_mask();
            let pad_len = max_len - ids.len();

            all_ids.extend_from_slice(ids);
            all_ids.extend(std::iter::repeat_n(0u32, pad_len));

            all_type_ids.extend_from_slice(type_ids);
            all_type_ids.extend(std::iter::repeat_n(0u32, pad_len));

            all_mask.extend_from_slice(mask);
            all_mask.extend(std::iter::repeat_n(0u32, pad_len));
        }

        let batch_size = encodings.len();
        let input_ids = Tensor::from_vec(all_ids, (batch_size, max_len), &self.device)?;
        let token_type_ids = Tensor::from_vec(all_type_ids, (batch_size, max_len), &self.device)?;
        let attention_mask =
            Tensor::from_vec(all_mask.clone(), (batch_size, max_len), &self.device)?;

        let output = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // Mean pooling
        let mask_f32 = Tensor::from_vec(
            all_mask.iter().map(|&m| m as f32).collect::<Vec<_>>(),
            (batch_size, max_len),
            &self.device,
        )?
        .unsqueeze(2)?;

        let masked = output.broadcast_mul(&mask_f32)?;
        let summed = masked.sum(1)?;
        let counts = mask_f32.sum(1)?;
        let pooled = summed.broadcast_div(&counts)?;

        // L2 normalize
        let norms = pooled.sqr()?.sum(1)?.sqrt()?.unsqueeze(1)?;
        let normalized = pooled.broadcast_div(&norms)?;

        let mut results = Vec::with_capacity(batch_size);
        for i in 0..batch_size {
            let vec = normalized.get(i)?.to_vec1::<f32>()?;
            results.push(vec);
        }

        Ok(results)
    }
}

impl Embedder for CandleEmbedder {
    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        // Process in batches to manage memory
        let batch_size = 32;
        let mut all_results = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(batch_size) {
            let results = self.embed_batch(chunk)?;
            all_results.extend(results);
        }
        Ok(all_results)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text])?;
        results
            .into_iter()
            .next()
            .context("expected one embedding result")
    }

    fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }
}
