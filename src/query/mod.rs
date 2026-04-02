use anyhow::Result;

use crate::embed::Embedder;
use crate::store::{Chunk, Store};

pub struct QueryResult {
    pub chunks: Vec<Chunk>,
}

pub fn query(
    _store: &dyn Store,
    _embedder: &dyn Embedder,
    _query: &str,
    _top_k: usize,
    _source: Option<&str>,
) -> Result<QueryResult> {
    todo!()
}
