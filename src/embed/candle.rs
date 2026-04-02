use anyhow::Result;

use super::Embedder;

pub struct CandleEmbedder {
    _model_path: std::path::PathBuf,
}

impl CandleEmbedder {
    pub fn load(_model_path: &std::path::Path) -> Result<Self> {
        todo!("load candle model")
    }
}

impl Embedder for CandleEmbedder {
    fn embed_passages(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        todo!()
    }

    fn embed_query(&self, _text: &str) -> Result<Vec<f32>> {
        todo!()
    }
}
