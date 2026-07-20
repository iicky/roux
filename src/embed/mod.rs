pub mod candle;

use anyhow::Result;

pub trait Embedder: Send + Sync {
    fn embed_passages(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;
    fn embed_query(&self, text: &str) -> Result<Vec<f32>>;
    fn embedding_dim(&self) -> usize;
}
