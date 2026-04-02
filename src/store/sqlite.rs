use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, ffi::sqlite3_auto_extension, params};

use super::{Chunk, SourceRecord, Store};

const EMBEDDING_DIM: usize = 384;

#[allow(clippy::missing_transmute_annotations)]
fn register_sqlite_vec() {
    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self> {
        register_sqlite_vec();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        let store = Self { conn };
        store.create_tables()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.create_tables()?;
        Ok(store)
    }

    fn create_tables(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS chunks (
                id             TEXT PRIMARY KEY,
                source_name    TEXT NOT NULL,
                source_version TEXT NOT NULL,
                language       TEXT NOT NULL,
                item_type      TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                signature      TEXT,
                doc            TEXT NOT NULL,
                body           TEXT NOT NULL,
                url            TEXT,
                ingested_at    INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_chunks_source
                ON chunks(source_name);

            CREATE TABLE IF NOT EXISTS sources (
                name           TEXT PRIMARY KEY,
                version        TEXT NOT NULL,
                language       TEXT NOT NULL,
                lockfile_hash  TEXT,
                ingested_at    INTEGER NOT NULL
            );",
        )?;

        // Create virtual table for vector search
        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
                id TEXT PRIMARY KEY,
                embedding float[{EMBEDDING_DIM}]
            );"
        ))?;

        Ok(())
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

impl Store for SqliteStore {
    fn upsert_chunks(&self, chunks: &[Chunk]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        {
            let mut chunk_stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO chunks
                    (id, source_name, source_version, language, item_type,
                     qualified_name, signature, doc, body, url, ingested_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;

            let mut vec_del_stmt = tx.prepare_cached("DELETE FROM vec_chunks WHERE id = ?1")?;

            let mut vec_ins_stmt =
                tx.prepare_cached("INSERT INTO vec_chunks (id, embedding) VALUES (?1, ?2)")?;

            for chunk in chunks {
                let now = Self::now();
                chunk_stmt.execute(params![
                    chunk.id,
                    chunk.source_name,
                    chunk.source_version,
                    chunk.language,
                    chunk.item_type,
                    chunk.qualified_name,
                    chunk.signature,
                    chunk.doc,
                    chunk.body,
                    chunk.url,
                    now,
                ])?;

                // sqlite-vec expects raw f32 bytes
                let embedding_bytes: Vec<u8> = chunk
                    .embedding
                    .iter()
                    .flat_map(|f| f.to_le_bytes())
                    .collect();
                vec_del_stmt.execute(params![chunk.id])?;
                vec_ins_stmt.execute(params![chunk.id, embedding_bytes])?;
            }

            // Update source record
            if let Some(first) = chunks.first() {
                tx.execute(
                    "INSERT OR REPLACE INTO sources (name, version, language, ingested_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        first.source_name,
                        first.source_version,
                        first.language,
                        Self::now(),
                    ],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    fn search(&self, embedding: &[f32], limit: usize, source: Option<&str>) -> Result<Vec<Chunk>> {
        let query_bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();

        // Get nearest IDs from vec_chunks
        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, distance
                 FROM vec_chunks
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )?;
            stmt.query_map(params![query_bytes, limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        if ids.is_empty() {
            return Ok(vec![]);
        }

        // Fetch full chunk metadata for each ID, preserving distance order
        let mut chunks = Vec::with_capacity(ids.len());
        for id in &ids {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id, source_name, source_version, language, item_type,
                        qualified_name, signature, doc, body, url, ingested_at
                 FROM chunks WHERE id = ?1",
            )?;
            let chunk = stmt.query_row(params![id], |row| {
                Ok(Chunk {
                    id: row.get(0)?,
                    source_name: row.get(1)?,
                    source_version: row.get(2)?,
                    language: row.get(3)?,
                    item_type: row.get(4)?,
                    qualified_name: row.get(5)?,
                    signature: row.get(6)?,
                    doc: row.get(7)?,
                    body: row.get(8)?,
                    embedding: vec![], // not loaded on search
                    url: row.get(9)?,
                    ingested_at: row.get(10)?,
                })
            })?;

            if let Some(src) = source {
                if chunk.source_name == src {
                    chunks.push(chunk);
                }
            } else {
                chunks.push(chunk);
            }
        }

        Ok(chunks)
    }

    fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.version, s.language, s.lockfile_hash, s.ingested_at,
                    COUNT(c.id) as chunk_count
             FROM sources s
             LEFT JOIN chunks c ON c.source_name = s.name
             GROUP BY s.name
             ORDER BY s.name",
        )?;

        let records = stmt
            .query_map([], |row| {
                Ok(SourceRecord {
                    name: row.get(0)?,
                    version: row.get(1)?,
                    language: row.get(2)?,
                    lockfile_hash: row.get(3)?,
                    ingested_at: row.get(4)?,
                    chunk_count: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(records)
    }

    fn remove_source(&self, name: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        // Get chunk IDs to remove from vec table
        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM chunks WHERE source_name = ?1")?;
            stmt.query_map(params![name], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        for id in &ids {
            tx.execute("DELETE FROM vec_chunks WHERE id = ?1", params![id])?;
        }

        tx.execute("DELETE FROM chunks WHERE source_name = ?1", params![name])?;
        tx.execute("DELETE FROM sources WHERE name = ?1", params![name])?;

        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_chunk(name: &str, qualified: &str) -> Chunk {
        Chunk {
            id: format!("test-{qualified}"),
            source_name: name.to_string(),
            source_version: "1.0.0".to_string(),
            language: "rust".to_string(),
            item_type: "function".to_string(),
            qualified_name: qualified.to_string(),
            signature: Some("fn example()".to_string()),
            doc: "A test chunk".to_string(),
            body: format!("function: {qualified}\nfn example()\nA test chunk"),
            embedding: vec![0.0; EMBEDDING_DIM],
            url: None,
            ingested_at: 0,
        }
    }

    #[test]
    fn test_open_in_memory() {
        let store = SqliteStore::open_in_memory().unwrap();
        let sources = store.list_sources().unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn test_upsert_and_list() {
        let store = SqliteStore::open_in_memory().unwrap();
        let chunks = vec![
            make_chunk("tokio", "tokio::spawn"),
            make_chunk("tokio", "tokio::sleep"),
        ];

        store.upsert_chunks(&chunks).unwrap();

        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "tokio");
        assert_eq!(sources[0].chunk_count, 2);
    }

    #[test]
    fn test_remove_source() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .upsert_chunks(&[make_chunk("tokio", "tokio::spawn")])
            .unwrap();

        store.remove_source("tokio").unwrap();

        let sources = store.list_sources().unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn test_search() {
        let store = SqliteStore::open_in_memory().unwrap();

        let mut c1 = make_chunk("tokio", "tokio::spawn");
        c1.embedding[0] = 1.0;

        let mut c2 = make_chunk("tokio", "tokio::sleep");
        c2.embedding[1] = 1.0;

        store.upsert_chunks(&[c1, c2]).unwrap();

        let mut query_vec = vec![0.0; EMBEDDING_DIM];
        query_vec[0] = 1.0;

        let results = store.search(&query_vec, 1, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].qualified_name, "tokio::spawn");
    }

    #[test]
    fn test_search_with_source_filter() {
        let store = SqliteStore::open_in_memory().unwrap();

        let mut c1 = make_chunk("tokio", "tokio::spawn");
        c1.embedding[0] = 1.0;

        let mut c2 = make_chunk("serde", "serde::Serialize");
        c2.embedding[0] = 0.9;
        c2.embedding[1] = 0.1;

        store.upsert_chunks(&[c1]).unwrap();
        store.upsert_chunks(&[c2]).unwrap();

        let mut query_vec = vec![0.0; EMBEDDING_DIM];
        query_vec[0] = 1.0;

        // Without filter: tokio::spawn is closest
        let results = store.search(&query_vec, 2, None).unwrap();
        assert_eq!(results.len(), 2);

        // With filter: only serde results
        let results = store.search(&query_vec, 2, Some("serde")).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_name, "serde");
    }

    #[test]
    fn test_search_empty_store() {
        let store = SqliteStore::open_in_memory().unwrap();
        let query_vec = vec![0.0; EMBEDDING_DIM];
        let results = store.search(&query_vec, 5, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn test_upsert_updates_existing() {
        let store = SqliteStore::open_in_memory().unwrap();

        let mut c1 = make_chunk("tokio", "tokio::spawn");
        c1.doc = "Original doc".to_string();
        store.upsert_chunks(&[c1]).unwrap();

        // Upsert same ID with different doc
        let mut c2 = make_chunk("tokio", "tokio::spawn");
        c2.doc = "Updated doc".to_string();
        c2.embedding[0] = 1.0;
        store.upsert_chunks(&[c2]).unwrap();

        // Should still be 1 chunk, not 2
        let sources = store.list_sources().unwrap();
        assert_eq!(sources[0].chunk_count, 1);

        // Search should find the updated chunk
        let mut query_vec = vec![0.0; EMBEDDING_DIM];
        query_vec[0] = 1.0;
        let results = store.search(&query_vec, 1, None).unwrap();
        assert_eq!(results[0].doc, "Updated doc");
    }

    #[test]
    fn test_multiple_sources() {
        let store = SqliteStore::open_in_memory().unwrap();

        store
            .upsert_chunks(&[
                make_chunk("tokio", "tokio::spawn"),
                make_chunk("tokio", "tokio::sleep"),
            ])
            .unwrap();
        store
            .upsert_chunks(&[make_chunk("serde", "serde::Serialize")])
            .unwrap();

        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 2);

        let tokio = sources.iter().find(|s| s.name == "tokio").unwrap();
        assert_eq!(tokio.chunk_count, 2);

        let serde = sources.iter().find(|s| s.name == "serde").unwrap();
        assert_eq!(serde.chunk_count, 1);
    }

    #[test]
    fn test_remove_nonexistent_source() {
        let store = SqliteStore::open_in_memory().unwrap();
        // Should not error
        store.remove_source("doesnt_exist").unwrap();
    }

    #[test]
    fn test_open_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");

        let store = SqliteStore::open(&db_path).unwrap();
        store
            .upsert_chunks(&[make_chunk("tokio", "tokio::spawn")])
            .unwrap();

        // Reopen and verify persistence
        drop(store);
        let store = SqliteStore::open(&db_path).unwrap();
        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
    }
}
