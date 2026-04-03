use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, ffi::sqlite3_auto_extension, params};

use super::{Chunk, SourceRecord, Store};

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
    embedding_dim: Option<usize>,
}

impl SqliteStore {
    /// Open a store with optional embedding support.
    /// Pass `Some(dim)` to enable vector search, `None` for FTS-only.
    pub fn open(path: &Path, embedding_dim: Option<usize>) -> Result<Self> {
        register_sqlite_vec();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        let store = Self {
            conn,
            embedding_dim,
        };
        store.create_tables()?;
        if let Some(dim) = embedding_dim {
            store.validate_or_set_dim(dim)?;
        }
        Ok(store)
    }

    /// Open an existing database, reading the embedding dimension from metadata if present.
    pub fn open_existing(path: &Path) -> Result<Self> {
        register_sqlite_vec();

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        let embedding_dim: Option<usize> = conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'embedding_dim'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse().ok());

        Ok(Self {
            conn,
            embedding_dim,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::open_in_memory_with_dim(Some(384))
    }

    pub fn open_in_memory_with_dim(embedding_dim: Option<usize>) -> Result<Self> {
        register_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn,
            embedding_dim,
        };
        store.create_tables()?;
        if let Some(dim) = embedding_dim {
            store.validate_or_set_dim(dim)?;
        }
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

        // Metadata table for storing embedding dimension and other config
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;

        // Create virtual table for vector search (only if embedding dim is known)
        if let Some(dim) = self.embedding_dim {
            self.conn.execute_batch(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
                    id TEXT PRIMARY KEY,
                    embedding float[{dim}]
                );"
            ))?;
        }

        // FTS5 full-text search index for keyword/identifier matching
        self.conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_chunks USING fts5(
                id UNINDEXED,
                qualified_name,
                signature,
                doc,
                body,
                tokenize='unicode61 remove_diacritics 2'
            );",
        )?;

        Ok(())
    }

    fn validate_or_set_dim(&self, dim: usize) -> Result<()> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'embedding_dim'",
                [],
                |row| row.get(0),
            )
            .ok();

        match stored {
            Some(val) => {
                let stored_dim: usize = val.parse().context("invalid embedding_dim in metadata")?;
                if stored_dim != dim {
                    anyhow::bail!(
                        "embedding dimension mismatch: database has {stored_dim}-dim embeddings, \
                         but current model produces {dim}-dim. Re-index with `roux remove` + `roux add`."
                    );
                }
            }
            None => {
                self.conn.execute(
                    "INSERT INTO metadata (key, value) VALUES ('embedding_dim', ?1)",
                    params![dim.to_string()],
                )?;
            }
        }
        Ok(())
    }

    /// Check if this store has embeddings enabled.
    pub fn has_embeddings(&self) -> bool {
        self.embedding_dim.is_some()
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

/// Escape a query string for safe use in FTS5 MATCH.
/// Wraps each word in double quotes to prevent FTS5 syntax errors from
/// special characters like colons, parentheses, etc.
fn fts_query_escape(query: &str) -> String {
    query
        .split_whitespace()
        .map(|word| {
            let clean: String = word
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if clean.is_empty() {
                String::new()
            } else {
                format!("\"{clean}\"")
            }
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" OR ")
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

            let mut fts_del_stmt = tx.prepare_cached("DELETE FROM fts_chunks WHERE id = ?1")?;
            let mut fts_ins_stmt = tx.prepare_cached(
                "INSERT INTO fts_chunks (id, qualified_name, signature, doc, body)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;

            // Only prepare vec statements if we have embeddings support
            let has_vec = self.embedding_dim.is_some();
            let mut vec_del_stmt = if has_vec {
                Some(tx.prepare_cached("DELETE FROM vec_chunks WHERE id = ?1")?)
            } else {
                None
            };
            let mut vec_ins_stmt = if has_vec {
                Some(tx.prepare_cached("INSERT INTO vec_chunks (id, embedding) VALUES (?1, ?2)")?)
            } else {
                None
            };

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

                // Insert embedding if present
                if let Some(ref embedding) = chunk.embedding {
                    let embedding_bytes: Vec<u8> =
                        embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
                    if let Some(ref mut del) = vec_del_stmt {
                        del.execute(params![chunk.id])?;
                    }
                    if let Some(ref mut ins) = vec_ins_stmt {
                        ins.execute(params![chunk.id, embedding_bytes])?;
                    }
                }

                // Update FTS index
                fts_del_stmt.execute(params![chunk.id])?;
                fts_ins_stmt.execute(params![
                    chunk.id,
                    chunk.qualified_name,
                    chunk.signature,
                    chunk.doc,
                    chunk.body,
                ])?;
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

    fn search(
        &self,
        embedding: Option<&[f32]>,
        query_text: &str,
        limit: usize,
        source: Option<&str>,
    ) -> Result<Vec<Chunk>> {
        // Over-fetch for both sources to feed into RRF
        let fetch_limit = if source.is_some() {
            limit * 4
        } else {
            limit * 2
        };

        // 1. Vector search: get ranked IDs by cosine distance (if embeddings available)
        let vec_ids: Vec<String> = if let Some(emb) = embedding {
            let query_bytes: Vec<u8> = emb.iter().flat_map(|f| f.to_le_bytes()).collect();
            let mut stmt = self.conn.prepare(
                "SELECT id, distance
                 FROM vec_chunks
                 WHERE embedding MATCH ?1
                 ORDER BY distance
                 LIMIT ?2",
            )?;
            stmt.query_map(params![query_bytes, fetch_limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            vec![]
        };

        // 2. FTS5 keyword search: get ranked IDs by BM25 relevance
        let fts_ids: Vec<String> = if !query_text.is_empty() {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM fts_chunks WHERE fts_chunks MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            // Escape FTS5 special characters for safe matching
            let safe_query = fts_query_escape(query_text);
            stmt.query_map(params![safe_query, fetch_limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            vec![]
        };

        // 3. Reciprocal Rank Fusion (k=60 is standard)
        let k = 60.0f64;
        let mut rrf_scores: std::collections::HashMap<String, f64> =
            std::collections::HashMap::new();

        for (rank, id) in vec_ids.iter().enumerate() {
            *rrf_scores.entry(id.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }
        for (rank, id) in fts_ids.iter().enumerate() {
            *rrf_scores.entry(id.clone()).or_default() += 1.0 / (k + rank as f64 + 1.0);
        }

        // Sort by RRF score descending
        let mut ranked: Vec<(String, f64)> = rrf_scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        if ranked.is_empty() {
            return Ok(vec![]);
        }

        // 4. Batch-fetch chunk metadata
        let fetch_ids: Vec<&str> = ranked.iter().map(|(id, _)| id.as_str()).collect();
        let placeholders: Vec<String> = (1..=fetch_ids.len()).map(|i| format!("?{i}")).collect();
        let in_clause = placeholders.join(", ");

        let sql = format!(
            "SELECT id, source_name, source_version, language, item_type,
                    qualified_name, signature, doc, body, url, ingested_at
             FROM chunks WHERE id IN ({in_clause})"
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let id_params: Vec<&dyn rusqlite::types::ToSql> = fetch_ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();

        let rows = stmt
            .query_map(id_params.as_slice(), |row| {
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
                    embedding: None,
                    url: row.get(9)?,
                    ingested_at: row.get(10)?,
                    score: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // 5. Reorder by RRF score, apply source filter
        let mut chunks = Vec::with_capacity(limit);
        for (id, rrf_score) in &ranked {
            if chunks.len() >= limit {
                break;
            }
            if let Some(mut chunk) = rows.iter().find(|c| c.id == *id).cloned() {
                if let Some(src) = source
                    && chunk.source_name != src
                {
                    continue;
                }
                chunk.score = Some(*rrf_score as f32);
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
            if self.embedding_dim.is_some() {
                let _ = tx.execute("DELETE FROM vec_chunks WHERE id = ?1", params![id]);
            }
            tx.execute("DELETE FROM fts_chunks WHERE id = ?1", params![id])?;
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

    const TEST_DIM: usize = 384;

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
            embedding: Some(vec![0.0; TEST_DIM]),
            url: None,
            ingested_at: 0,
            score: None,
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
        c1.embedding.as_mut().unwrap()[0] = 1.0;

        let mut c2 = make_chunk("tokio", "tokio::sleep");
        c2.embedding.as_mut().unwrap()[1] = 1.0;

        store.upsert_chunks(&[c1, c2]).unwrap();

        let mut query_vec = vec![0.0; TEST_DIM];
        query_vec[0] = 1.0;

        let results = store.search(Some(&query_vec), "", 1, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].qualified_name, "tokio::spawn");
    }

    #[test]
    fn test_search_with_source_filter() {
        let store = SqliteStore::open_in_memory().unwrap();

        let mut c1 = make_chunk("tokio", "tokio::spawn");
        c1.embedding.as_mut().unwrap()[0] = 1.0;

        let mut c2 = make_chunk("serde", "serde::Serialize");
        c2.embedding.as_mut().unwrap()[0] = 0.9;
        c2.embedding.as_mut().unwrap()[1] = 0.1;

        store.upsert_chunks(&[c1]).unwrap();
        store.upsert_chunks(&[c2]).unwrap();

        let mut query_vec = vec![0.0; TEST_DIM];
        query_vec[0] = 1.0;

        // Without filter: tokio::spawn is closest
        let results = store.search(Some(&query_vec), "", 2, None).unwrap();
        assert_eq!(results.len(), 2);

        // With filter: only serde results
        let results = store
            .search(Some(&query_vec), "", 2, Some("serde"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_name, "serde");
    }

    #[test]
    fn test_search_empty_store() {
        let store = SqliteStore::open_in_memory().unwrap();
        let query_vec = vec![0.0; TEST_DIM];
        let results = store.search(Some(&query_vec), "", 5, None).unwrap();
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
        c2.embedding.as_mut().unwrap()[0] = 1.0;
        store.upsert_chunks(&[c2]).unwrap();

        // Should still be 1 chunk, not 2
        let sources = store.list_sources().unwrap();
        assert_eq!(sources[0].chunk_count, 1);

        // Search should find the updated chunk
        let mut query_vec = vec![0.0; TEST_DIM];
        query_vec[0] = 1.0;
        let results = store.search(Some(&query_vec), "", 1, None).unwrap();
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

        let store = SqliteStore::open(&db_path, Some(TEST_DIM)).unwrap();
        store
            .upsert_chunks(&[make_chunk("tokio", "tokio::spawn")])
            .unwrap();

        // Reopen and verify persistence
        drop(store);
        let store = SqliteStore::open(&db_path, Some(TEST_DIM)).unwrap();
        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn test_dim_mismatch_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");

        // Create with dim 384
        let store = SqliteStore::open(&db_path, Some(384)).unwrap();
        drop(store);

        // Reopen with different dim should fail
        let result = SqliteStore::open(&db_path, Some(768));
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(
            err.contains("dimension mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_open_existing_reads_dim() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.sqlite");

        let store = SqliteStore::open(&db_path, Some(TEST_DIM)).unwrap();
        store
            .upsert_chunks(&[make_chunk("tokio", "tokio::spawn")])
            .unwrap();
        drop(store);

        let store = SqliteStore::open_existing(&db_path).unwrap();
        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn test_search_returns_scores() {
        let store = SqliteStore::open_in_memory().unwrap();

        let mut c1 = make_chunk("tokio", "tokio::spawn");
        c1.embedding.as_mut().unwrap()[0] = 1.0;

        store.upsert_chunks(&[c1]).unwrap();

        let mut query_vec = vec![0.0; TEST_DIM];
        query_vec[0] = 1.0;

        let results = store.search(Some(&query_vec), "", 1, None).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].score.is_some());
        // Same vector should have high similarity
        let score = results[0].score.unwrap();
        assert!(score > 0.0, "expected positive score, got {score}");
    }

    #[test]
    fn test_source_filter_backfill() {
        let store = SqliteStore::open_in_memory().unwrap();

        // Insert several chunks from different sources
        for i in 0..10 {
            let mut c = make_chunk("other", &format!("other::fn_{i}"));
            c.id = format!("other-{i}");
            c.embedding.as_mut().unwrap()[i % TEST_DIM] = 1.0;
            store.upsert_chunks(&[c]).unwrap();
        }

        let mut target = make_chunk("target", "target::find_me");
        target.id = "target-1".to_string();
        target.embedding.as_mut().unwrap()[0] = 0.9;
        target.embedding.as_mut().unwrap()[1] = 0.1;
        store.upsert_chunks(&[target]).unwrap();

        let mut query_vec = vec![0.0; TEST_DIM];
        query_vec[0] = 1.0;

        // With source filter, should still find the target even though
        // other chunks are closer
        let results = store
            .search(Some(&query_vec), "", 5, Some("target"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_name, "target");
    }

    #[test]
    fn test_hybrid_search_keyword_boost() {
        let store = SqliteStore::open_in_memory().unwrap();

        // Two chunks with same embedding but different text
        let mut c1 = make_chunk("lib", "lib::HashMap");
        c1.doc = "A hash map implementation".to_string();
        c1.embedding.as_mut().unwrap()[0] = 1.0;

        let mut c2 = make_chunk("lib", "lib::TreeMap");
        c2.doc = "A tree-based sorted map".to_string();
        c2.embedding.as_mut().unwrap()[0] = 0.99;
        c2.embedding.as_mut().unwrap()[1] = 0.01;

        store.upsert_chunks(&[c1, c2]).unwrap();

        // Vector-only: both are very close
        let mut query_vec = vec![0.0; TEST_DIM];
        query_vec[0] = 1.0;

        // Hybrid with keyword "HashMap" should boost c1
        let results = store.search(Some(&query_vec), "HashMap", 2, None).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].qualified_name, "lib::HashMap",
            "keyword match should be ranked first"
        );
    }

    #[test]
    fn test_fts_query_escape() {
        assert_eq!(fts_query_escape("hello world"), "\"hello\" OR \"world\"");
        assert_eq!(
            fts_query_escape("HashMap::from_iter"),
            "\"HashMapfrom_iter\""
        );
        assert_eq!(fts_query_escape(""), "");
        // Special chars stripped
        assert_eq!(fts_query_escape("fn() -> bool"), "\"fn\" OR \"bool\"");
    }
}
