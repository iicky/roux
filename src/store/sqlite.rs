use anyhow::Result;

use super::{Chunk, SourceRecord, Store};

pub struct SqliteStore {
    _conn: rusqlite::Connection,
}

impl SqliteStore {
    pub fn open(_path: &std::path::Path) -> Result<Self> {
        todo!("open sqlite store")
    }
}

impl Store for SqliteStore {
    fn upsert_chunks(&self, _chunks: &[Chunk]) -> Result<()> {
        todo!()
    }

    fn search(
        &self,
        _embedding: &[f32],
        _limit: usize,
        _source: Option<&str>,
    ) -> Result<Vec<Chunk>> {
        todo!()
    }

    fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        todo!()
    }

    fn remove_source(&self, _name: &str) -> Result<()> {
        todo!()
    }
}
