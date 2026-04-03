pub mod sqlite;

use anyhow::Result;

#[derive(Clone)]
pub struct Chunk {
    pub id: String,
    pub source_name: String,
    pub source_version: String,
    pub language: String,
    pub item_type: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub doc: String,
    pub body: String,
    pub embedding: Vec<f32>,
    pub url: Option<String>,
    pub ingested_at: i64,
    /// Similarity score from search (1.0 = identical, 0.0 = orthogonal).
    /// Only populated on search results.
    pub score: Option<f32>,
}

pub trait Store {
    fn upsert_chunks(&self, chunks: &[Chunk]) -> Result<()>;
    fn search(&self, embedding: &[f32], limit: usize, source: Option<&str>) -> Result<Vec<Chunk>>;
    fn list_sources(&self) -> Result<Vec<SourceRecord>>;
    fn remove_source(&self, name: &str) -> Result<()>;
}

pub struct SourceRecord {
    pub name: String,
    pub version: String,
    pub language: String,
    pub chunk_count: usize,
    pub ingested_at: i64,
    pub lockfile_hash: Option<String>,
}
