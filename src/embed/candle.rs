use std::path::Path;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use tokenizers::Tokenizer;

use super::Embedder;

pub struct CandleEmbedder {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl CandleEmbedder {
    pub fn load(model_path: &Path, tokenizer_path: &Path, config_path: &Path) -> Result<Self> {
        let device = Device::Cpu;

        // Load config
        let config_str =
            std::fs::read_to_string(config_path).context("reading model config.json")?;
        let config: BertConfig =
            serde_json::from_str(&config_str).context("parsing model config.json")?;

        // Load model weights
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[model_path], DType::F32, &device)
                .context("loading model weights")?
        };
        let model = BertModel::load(vb, &config).context("building BERT model")?;

        // Load tokenizer with truncation to model's max length
        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        let truncation = tokenizers::TruncationParams {
            max_length: 512,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(truncation))
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embed a batch of texts with the given prefix.
    fn embed_batch(&self, texts: &[&str], prefix: &str) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
        let prefixed_refs: Vec<&str> = prefixed.iter().map(|s| s.as_str()).collect();

        let encodings = self
            .tokenizer
            .encode_batch(prefixed_refs, true)
            .map_err(|e| anyhow::anyhow!("tokenization failed: {e}"))?;

        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        // Build padded input tensors
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

        // Forward pass
        let output = self
            .model
            .forward(&input_ids, &token_type_ids, Some(&attention_mask))?;

        // Mean pooling: sum(token_embeddings * mask) / sum(mask)
        let mask_f32 = Tensor::from_vec(
            all_mask.iter().map(|&m| m as f32).collect::<Vec<_>>(),
            (batch_size, max_len),
            &self.device,
        )?
        .unsqueeze(2)?; // (batch, seq, 1)

        let masked = output.broadcast_mul(&mask_f32)?;
        let summed = masked.sum(1)?; // (batch, hidden)
        let counts = mask_f32.sum(1)?; // (batch, 1)
        let pooled = summed.broadcast_div(&counts)?;

        // L2 normalize
        let norms = pooled.sqr()?.sum(1)?.sqrt()?.unsqueeze(1)?;
        let normalized = pooled.broadcast_div(&norms)?;

        // Extract as Vec<Vec<f32>>
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
        self.embed_batch(texts, "passage: ")
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text], "query: ")?;
        results
            .into_iter()
            .next()
            .context("expected one embedding result")
    }
}
