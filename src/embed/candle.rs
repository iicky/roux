use std::path::Path;

use anyhow::{Context, Result};
use candle_core::{DType, Device, Module, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config as BertConfig};
use candle_transformers::models::jina_bert::{BertModel as JinaBertModel, Config as JinaConfig};
use tokenizers::Tokenizer;

use super::{Embedder, ensure_model};

/// Loaded model backend. E5 is a standard BERT (learned positions, needs
/// query/passage prefixes); Jina is an ALiBi BERT (positions implicit, no
/// segment ids or attention mask in the forward, no instruction prefix).
enum Model {
    Bert(BertModel),
    Jina(JinaBertModel),
}

pub struct CandleEmbedder {
    model: Model,
    tokenizer: Tokenizer,
    device: Device,
    query_prefix: &'static str,
    passage_prefix: &'static str,
}

impl CandleEmbedder {
    /// Load a model by HuggingFace id, downloading it to the local cache on
    /// first use. Architecture and prefixes are selected from the id.
    pub fn from_pretrained(model_id: &str) -> Result<Self> {
        let files = ensure_model(model_id)?;
        let is_jina = model_id.to_lowercase().contains("jina");
        Self::load(&files.model, &files.tokenizer, &files.config, is_jina)
    }

    pub fn load(
        model_path: &Path,
        tokenizer_path: &Path,
        config_path: &Path,
        is_jina: bool,
    ) -> Result<Self> {
        let device = Self::select_device();

        let config_str =
            std::fs::read_to_string(config_path).context("reading model config.json")?;
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[model_path], DType::F32, &device)
                .context("loading model weights")?
        };

        let (model, query_prefix, passage_prefix) = if is_jina {
            let config: JinaConfig =
                serde_json::from_str(&config_str).context("parsing jina config.json")?;
            let model = JinaBertModel::new(vb, &config).context("building Jina BERT model")?;
            // Jina v2 embeddings use raw text — no instruction prefix.
            (Model::Jina(model), "", "")
        } else {
            let config: BertConfig =
                serde_json::from_str(&config_str).context("parsing bert config.json")?;
            let model = BertModel::load(vb, &config).context("building BERT model")?;
            // E5 expects asymmetric query/passage instruction prefixes.
            (Model::Bert(model), "query: ", "passage: ")
        };

        // Enforce the position limit so an oversized symbol body can't overflow.
        let mut tokenizer =
            Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("{e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: 512,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("setting truncation: {e}"))?;

        Ok(Self {
            model,
            tokenizer,
            device,
            query_prefix,
            passage_prefix,
        })
    }

    /// Pick the best available compute device for the build's features,
    /// falling back to CPU if the GPU backend can't be initialized.
    fn select_device() -> Device {
        #[cfg(feature = "metal")]
        {
            match Device::new_metal(0) {
                Ok(d) => return d,
                Err(e) => eprintln!("Metal unavailable ({e}), falling back to CPU"),
            }
        }
        #[cfg(feature = "cuda")]
        {
            match Device::new_cuda(0) {
                Ok(d) => return d,
                Err(e) => eprintln!("CUDA unavailable ({e}), falling back to CPU"),
            }
        }
        Device::Cpu
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

        // Forward pass — backend-specific signature.
        let output = match &self.model {
            Model::Bert(m) => {
                let token_type_ids =
                    Tensor::from_vec(all_type_ids, (batch_size, max_len), &self.device)?;
                let attention_mask =
                    Tensor::from_vec(all_mask.clone(), (batch_size, max_len), &self.device)?;
                m.forward(&input_ids, &token_type_ids, Some(&attention_mask))?
            }
            // Jina (ALiBi) forward takes only input_ids; pads are excluded in pooling.
            Model::Jina(m) => m.forward(&input_ids)?,
        };

        // Mean pooling: sum(token_embeddings * mask) / sum(mask)
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
        self.embed_batch(texts, self.passage_prefix)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        let results = self.embed_batch(&[text], self.query_prefix)?;
        results
            .into_iter()
            .next()
            .context("expected one embedding result")
    }
}
