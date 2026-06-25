use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::{Edge, Node};

pub struct GraphStore {
    conn: Connection,
}

impl GraphStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {}", parent.display()))?;
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // No FK enforcement — graph references are resolved best-effort
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        // Check schema version
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;

        let version: i64 = self
            .conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if version < 4 {
            // Drop old tables if they exist (pre-v1 data)
            self.conn.execute_batch(
                "DROP TABLE IF EXISTS fts_symbols;
                 DROP TABLE IF EXISTS fts_nodes;
                 DROP TABLE IF EXISTS fts_chunks;
                 DROP TABLE IF EXISTS vec_chunks;
                 DROP TABLE IF EXISTS edges;
                 DROP TABLE IF EXISTS symbols;
                 DROP TABLE IF EXISTS nodes;
                 DROP TABLE IF EXISTS chunks;
                 DROP TABLE IF EXISTS sources;",
            )?;

            self.conn.execute_batch(
                "CREATE TABLE sources (
                    name        TEXT PRIMARY KEY,
                    version     TEXT NOT NULL,
                    language    TEXT NOT NULL,
                    ingested_at INTEGER NOT NULL
                );

                CREATE TABLE nodes (
                    id             TEXT PRIMARY KEY,
                    kind           TEXT NOT NULL,
                    name           TEXT NOT NULL,
                    qualified_name TEXT NOT NULL,
                    source_name    TEXT NOT NULL,
                    language       TEXT NOT NULL,
                    file_path      TEXT NOT NULL,
                    start_line     INTEGER NOT NULL,
                    start_col      INTEGER NOT NULL DEFAULT 0,
                    end_line       INTEGER NOT NULL DEFAULT 0,
                    visibility     TEXT NOT NULL DEFAULT '',
                    signature      TEXT,
                    doc            TEXT,
                    body           TEXT NOT NULL DEFAULT '',
                    parent_id      TEXT,
                    content_hash   TEXT,
                    line_count     INTEGER NOT NULL DEFAULT 0,
                    source_url     TEXT,
                    description    TEXT
                );

                CREATE INDEX idx_nodes_source    ON nodes(source_name);
                CREATE INDEX idx_nodes_name      ON nodes(name);
                CREATE INDEX idx_nodes_kind      ON nodes(kind);
                CREATE INDEX idx_nodes_file_path ON nodes(file_path);
                CREATE INDEX idx_nodes_parent    ON nodes(parent_id);

                CREATE TABLE edges (
                    from_id TEXT NOT NULL ,
                    to_id   TEXT NOT NULL ,
                    kind    TEXT NOT NULL,
                    PRIMARY KEY (from_id, to_id, kind)
                );

                CREATE INDEX idx_edges_from ON edges(from_id);
                CREATE INDEX idx_edges_to   ON edges(to_id);
                CREATE INDEX idx_edges_kind ON edges(kind);

                CREATE VIRTUAL TABLE fts_nodes USING fts5(
                    id UNINDEXED,
                    name,
                    qualified_name,
                    file_path,
                    signature,
                    doc,
                    body,
                    tokenize='unicode61 remove_diacritics 2'
                );",
            )?;

            self.conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '5')",
                [],
            )?;
        }

        if (4..5).contains(&version) {
            // v4→v5 historically added a `vectors` table for the (now removed)
            // embedding pipeline; the table is unused, so this is a version bump.
            self.conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '5')",
                [],
            )?;
        }

        if version < 6 {
            // Staleness metadata: per-source origin and fingerprint.
            // source_kind: "crate", "path", "file", "url", or "" if unknown
            // origin: original input (crate name, local path, URL) the source was loaded from
            // fingerprint: a value that changes when the upstream changes — mtime/size rollup
            //              for path sources, file content hash for file sources, version for crates
            for sql in [
                "ALTER TABLE sources ADD COLUMN source_kind TEXT NOT NULL DEFAULT ''",
                "ALTER TABLE sources ADD COLUMN origin TEXT",
                "ALTER TABLE sources ADD COLUMN fingerprint TEXT",
            ] {
                // ignore "duplicate column" errors so repeated opens on already-migrated DBs work
                let _ = self.conn.execute(sql, []);
            }
            self.conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '6')",
                [],
            )?;
        }

        if version < 7 {
            // Pre-v7 ingestion runs left duplicate FTS rows behind on some DBs
            // (1:N rather than 1:1 with nodes), inflating matched_ids and
            // double-weighting PPR seeds. Heal in place by keeping the lowest
            // rowid per id; FTS body is identical across duplicates so the
            // surviving row is correct.
            self.conn.execute_batch(
                "DELETE FROM fts_nodes WHERE rowid NOT IN (
                     SELECT MIN(rowid) FROM fts_nodes GROUP BY id
                 );
                 INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '7');",
            )?;
        }

        Ok(())
    }

    /// Insert nodes and edges for a source, replacing any existing data.
    pub fn upsert_source(
        &self,
        source_name: &str,
        source_version: &str,
        language: &str,
        nodes: &[Node],
        edges: &[Edge],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        // Ensure source record exists (before FK-constrained node inserts)
        tx.execute(
            "INSERT OR REPLACE INTO sources (name, version, language, ingested_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![source_name, source_version, language, now()],
        )?;

        // Remove old data for this source. FTS5 deletes filtered on an
        // UNINDEXED column (`id`) don't reliably remove rows when the table
        // already contains entries for those ids, so route FTS deletion
        // through rowid — which is always indexed.
        {
            let ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM nodes WHERE source_name = ?1")?;
                stmt.query_map(params![source_name], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };

            let mut edge_del =
                tx.prepare_cached("DELETE FROM edges WHERE from_id = ?1 OR to_id = ?1")?;
            for id in &ids {
                edge_del.execute(params![id])?;
            }
            drop(edge_del);
            delete_fts_by_ids(&tx, &ids)?;
            tx.execute(
                "DELETE FROM nodes WHERE source_name = ?1",
                params![source_name],
            )?;
        }

        // Insert nodes
        {
            let mut node_stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO nodes
                    (id, kind, name, qualified_name, source_name, language,
                     file_path, start_line, start_col, end_line, visibility,
                     signature, doc, body, parent_id, content_hash, line_count, source_url, description)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
            )?;
            let mut fts_stmt = tx.prepare_cached(
                "INSERT INTO fts_nodes (id, name, qualified_name, file_path, signature, doc, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;

            for node in nodes {
                node_stmt.execute(params![
                    node.id,
                    node.kind,
                    node.name,
                    node.qualified_name,
                    node.source_name,
                    node.language,
                    node.file_path,
                    node.start_line,
                    node.start_col,
                    node.end_line,
                    node.visibility,
                    node.signature,
                    node.doc,
                    node.body,
                    node.parent_id,
                    node.content_hash,
                    node.line_count,
                    node.source_url,
                    node.description,
                ])?;
                // Index both original text AND tokenized form for best of both
                let fts_name = format!("{} {}", node.name, tokenize_for_fts(&node.name));
                let fts_qualified = format!(
                    "{} {}",
                    node.qualified_name,
                    tokenize_for_fts(&node.qualified_name)
                );
                let fts_body = format!("{} {}", node.body, tokenize_for_fts(&node.body));
                fts_stmt.execute(params![
                    node.id,
                    fts_name,
                    fts_qualified,
                    node.file_path,
                    node.signature,
                    node.doc,
                    fts_body,
                ])?;
            }
        }

        // Insert edges
        {
            let mut edge_stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO edges (from_id, to_id, kind) VALUES (?1, ?2, ?3)",
            )?;
            for edge in edges {
                edge_stmt.execute(params![edge.from_id, edge.to_id, edge.kind])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Search by keyword, return matching nodes + their graph neighborhood.
    pub fn search(&self, query: &str, limit: usize) -> Result<SearchResult> {
        self.search_with_opts(
            query,
            limit,
            super::rank::FusionMethod::ScoreFusion,
            true,
            None,
        )
    }

    pub fn search_with_fusion(
        &self,
        query: &str,
        limit: usize,
        fusion: super::rank::FusionMethod,
    ) -> Result<SearchResult> {
        self.search_with_opts(query, limit, fusion, true, None)
    }

    /// Search restricted to a named source. Returns an error if the source doesn't exist.
    pub fn search_scoped(
        &self,
        query: &str,
        limit: usize,
        source: Option<&str>,
    ) -> Result<SearchResult> {
        if let Some(src) = source {
            let exists: bool = self
                .conn
                .query_row(
                    "SELECT 1 FROM sources WHERE name = ?1",
                    params![src],
                    |_row| Ok(true),
                )
                .unwrap_or(false);
            if !exists {
                anyhow::bail!(
                    "source '{src}' not found. Run `roux list` to see available sources."
                );
            }
        }
        self.search_with_opts(query, limit, fusion_from_env(), true, source)
    }

    /// Run several queries and fuse their results — the agent-reformulation
    /// path. An LLM caller closes the vocabulary gap by reformulating one
    /// human question into N domain-jargon variants ("jolts" → "jerk",
    /// "CLASSIC_JERK", "M205"); firing them in one call and RRF-fusing the
    /// matched symbols recovers hits a single lexical query misses, with no
    /// embedding model. Falls back to a plain scoped search for a single query.
    pub fn search_multi(
        &self,
        queries: &[String],
        limit: usize,
        source: Option<&str>,
    ) -> Result<SearchResult> {
        let queries: Vec<&String> = queries.iter().filter(|q| !q.trim().is_empty()).collect();
        match queries.as_slice() {
            [] => return Ok(SearchResult::default()),
            [only] => return self.search_scoped(only, limit, source),
            _ => {}
        }

        // Reciprocal-rank fusion across each query's matched ordering. Pull a
        // wider slice per query (limit*2) so a symbol ranked modestly by several
        // variants can still win the fused top-k.
        const RRF_K: f64 = 60.0;
        let per_query = limit.saturating_mul(2).max(limit);
        let mut rrf: HashMap<String, f64> = HashMap::new();
        let mut node_map: HashMap<String, Node> = HashMap::new();
        let mut edge_set: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut edges: Vec<Edge> = Vec::new();

        for q in &queries {
            let res = self.search_scoped(q, per_query, source)?;
            for (rank, id) in res.matched_ids.iter().enumerate() {
                *rrf.entry(id.clone()).or_insert(0.0) += 1.0 / (RRF_K + (rank + 1) as f64);
            }
            for n in res.nodes {
                node_map.entry(n.id.clone()).or_insert(n);
            }
            for e in res.edges {
                let key = (e.from_id.clone(), e.to_id.clone(), e.kind.clone());
                if edge_set.insert(key) {
                    edges.push(e);
                }
            }
        }

        // Fused matched set: top-`limit` symbols by RRF score.
        let mut fused: Vec<(String, f64)> = rrf.into_iter().collect();
        fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let matched_ids: Vec<String> =
            fused.iter().take(limit).map(|(id, _)| id.clone()).collect();
        let scores: HashMap<String, f64> = fused.into_iter().collect();

        // Output nodes: matched first (RRF order), then their neighborhood.
        let mut nodes: Vec<Node> = Vec::new();
        for id in &matched_ids {
            if let Some(n) = node_map.remove(id) {
                nodes.push(n);
            }
        }
        let mut neighbors: Vec<Node> = node_map.into_values().collect();
        neighbors.sort_by(|a, b| a.id.cmp(&b.id));
        nodes.extend(neighbors);

        let kept: std::collections::HashSet<&String> = nodes.iter().map(|n| &n.id).collect();
        edges.retain(|e| kept.contains(&e.from_id) && kept.contains(&e.to_id));

        Ok(SearchResult {
            matched_ids,
            nodes,
            edges,
            scores,
        })
    }

    pub fn search_with_opts(
        &self,
        query: &str,
        limit: usize,
        fusion: super::rank::FusionMethod,
        desc_rerank: bool,
        source: Option<&str>,
    ) -> Result<SearchResult> {
        let safe_query = fts_query_escape(query);
        if safe_query.is_empty() {
            return Ok(SearchResult::default());
        }

        // BM25 search on FTS index — capture scores. When a source filter is
        // set, join against nodes to restrict matches to that source.
        let bm25_results: Vec<(String, f64)> = if let Some(src) = source {
            let mut stmt = self.conn.prepare(
                "SELECT f.id, f.rank FROM fts_nodes f
                 JOIN nodes n ON n.id = f.id
                 WHERE fts_nodes MATCH ?1 AND n.source_name = ?2
                 ORDER BY f.rank LIMIT ?3",
            )?;
            stmt.query_map(params![safe_query, src, (limit * 2) as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, rank FROM fts_nodes WHERE fts_nodes MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            stmt.query_map(params![safe_query, (limit * 2) as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let matched_ids: Vec<String> = bm25_results.iter().map(|(id, _)| id.clone()).collect();

        // Normalize BM25 scores to [0,1] (rank is negative, lower = better)
        let bm25_scores: HashMap<String, f64> = if bm25_results.is_empty() {
            HashMap::new()
        } else {
            let min_rank = bm25_results
                .iter()
                .map(|(_, r)| *r)
                .fold(f64::INFINITY, f64::min);
            let max_rank = bm25_results
                .iter()
                .map(|(_, r)| *r)
                .fold(f64::NEG_INFINITY, f64::max);
            let range = (max_rank - min_rank).max(1e-10);
            bm25_results
                .iter()
                .map(|(id, rank)| {
                    // Invert: lower rank = higher score
                    let normalized = 1.0 - (rank - min_rank) / range;
                    (id.clone(), normalized)
                })
                .collect()
        };

        if matched_ids.is_empty() {
            return Ok(SearchResult::default());
        }

        // Pull 2-hop ego-graph around seed nodes via recursive expansion
        let mut all_ids: Vec<String> = matched_ids.clone();

        // Hop 1 + Hop 2: expand edges + parent/children for each seed
        for _hop in 0..2 {
            let mut new_ids = Vec::new();
            for id in &all_ids {
                // Edge neighbors (both directions)
                let mut stmt = self.conn.prepare_cached(
                    "SELECT to_id FROM edges WHERE from_id = ?1
                     UNION
                     SELECT from_id FROM edges WHERE to_id = ?1",
                )?;
                let neighbors: Vec<String> = stmt
                    .query_map(params![id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                new_ids.extend(neighbors);

                // Parent
                if let Ok(pid) = self.conn.query_row(
                    "SELECT parent_id FROM nodes WHERE id = ?1 AND parent_id IS NOT NULL",
                    params![id],
                    |row| row.get::<_, String>(0),
                ) {
                    new_ids.push(pid);
                }

                // Children
                let mut stmt = self
                    .conn
                    .prepare_cached("SELECT id FROM nodes WHERE parent_id = ?1")?;
                let children: Vec<String> = stmt
                    .query_map(params![id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                new_ids.extend(children);
            }
            all_ids.extend(new_ids);
            all_ids.sort();
            all_ids.dedup();
        }

        // Fetch full subgraph
        let nodes = self.fetch_nodes(&all_ids)?;
        let edges = self.fetch_edges(&all_ids)?;

        // Run PPR ranking on the subgraph, fused with BM25 scores
        let q = if desc_rerank { Some(query) } else { None };
        let ranked = super::rank::rank_subgraph_with(
            nodes,
            edges,
            &matched_ids,
            &bm25_scores,
            limit,
            fusion,
            q,
        );

        let scores: HashMap<String, f64> = ranked
            .nodes
            .iter()
            .map(|sn| (sn.node.id.clone(), sn.score))
            .collect();

        let mut out_nodes: Vec<Node> = ranked.nodes.into_iter().map(|sn| sn.node).collect();
        let mut out_edges = ranked.edges;

        // Apply source filter to final output: expansion may have pulled in
        // cross-source neighbors; the user asked for a specific source only.
        if let Some(src) = source {
            out_nodes.retain(|n| n.source_name == src);
            let kept_ids: std::collections::HashSet<&String> =
                out_nodes.iter().map(|n| &n.id).collect();
            out_edges.retain(|e| kept_ids.contains(&e.from_id) && kept_ids.contains(&e.to_id));
        }

        Ok(SearchResult {
            matched_ids,
            nodes: out_nodes,
            edges: out_edges,
            scores,
        })
    }

    pub fn fetch_nodes(&self, ids: &[String]) -> Result<Vec<Node>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut all_nodes = Vec::new();
        // Batch to stay under SQLite's 32766 parameter limit
        for chunk in ids.chunks(20000) {
            let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT id, kind, name, qualified_name, source_name, language,
                        file_path, start_line, start_col, end_line, visibility,
                        signature, doc, body, parent_id, content_hash, line_count, source_url, description
                 FROM nodes WHERE id IN ({})",
                placeholders.join(", ")
            );

            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                .iter()
                .map(|id| id as &dyn rusqlite::types::ToSql)
                .collect();

            let nodes = stmt
                .query_map(params.as_slice(), |row| {
                    Ok(Node {
                        id: row.get(0)?,
                        kind: row.get(1)?,
                        name: row.get(2)?,
                        qualified_name: row.get(3)?,
                        source_name: row.get(4)?,
                        language: row.get(5)?,
                        file_path: row.get(6)?,
                        start_line: row.get(7)?,
                        start_col: row.get(8)?,
                        end_line: row.get(9)?,
                        visibility: row.get(10)?,
                        signature: row.get(11)?,
                        doc: row.get(12)?,
                        body: row.get(13)?,
                        parent_id: row.get(14)?,
                        content_hash: row.get(15)?,
                        line_count: row.get(16)?,
                        source_url: row.get(17)?,
                        description: row.get(18)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all_nodes.extend(nodes);
        }

        Ok(all_nodes)
    }

    fn fetch_edges(&self, ids: &[String]) -> Result<Vec<Edge>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let mut all_edges = Vec::new();
        // Batch to stay under SQLite's 32766 parameter limit (ids appear twice: from + to)
        for chunk in ids.chunks(10000) {
            let n = chunk.len();
            let ph1: Vec<String> = (1..=n).map(|i| format!("?{i}")).collect();
            let ph2: Vec<String> = (n + 1..=n * 2).map(|i| format!("?{i}")).collect();
            let sql = format!(
                "SELECT from_id, to_id, kind FROM edges
                 WHERE from_id IN ({}) OR to_id IN ({})",
                ph1.join(", "),
                ph2.join(", ")
            );

            let mut stmt = self.conn.prepare(&sql)?;
            let mut all_params: Vec<&dyn rusqlite::types::ToSql> = Vec::with_capacity(n * 2);
            for id in chunk {
                all_params.push(id as &dyn rusqlite::types::ToSql);
            }
            for id in chunk {
                all_params.push(id as &dyn rusqlite::types::ToSql);
            }

            let edges = stmt
                .query_map(all_params.as_slice(), |row| {
                    Ok(Edge {
                        from_id: row.get(0)?,
                        to_id: row.get(1)?,
                        kind: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all_edges.extend(edges);
        }

        Ok(all_edges)
    }

    /// Compare stored content hashes against new nodes to find what changed.
    /// Returns (added, modified, removed) node IDs.
    pub fn diff_source(
        &self,
        source_name: &str,
        new_nodes: &[super::Node],
    ) -> Result<(Vec<String>, Vec<String>, Vec<String>)> {
        // Get stored hashes
        let mut stmt = self
            .conn
            .prepare("SELECT id, content_hash FROM nodes WHERE source_name = ?1")?;
        let stored: HashMap<String, Option<String>> = stmt
            .query_map(params![source_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;

        let mut added = Vec::new();
        let mut modified = Vec::new();
        let new_ids: std::collections::HashSet<&str> =
            new_nodes.iter().map(|n| n.id.as_str()).collect();

        for node in new_nodes {
            match stored.get(&node.id) {
                None => added.push(node.id.clone()),
                Some(old_hash) => {
                    if old_hash.as_deref() != node.content_hash.as_deref() {
                        modified.push(node.id.clone());
                    }
                }
            }
        }

        let removed: Vec<String> = stored
            .keys()
            .filter(|id| !new_ids.contains(id.as_str()))
            .cloned()
            .collect();

        Ok((added, modified, removed))
    }

    pub fn remove_source(&self, name: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM nodes WHERE source_name = ?1")?;
            stmt.query_map(params![name], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let mut edge_del =
            tx.prepare_cached("DELETE FROM edges WHERE from_id = ?1 OR to_id = ?1")?;
        for id in &ids {
            edge_del.execute(params![id])?;
        }
        drop(edge_del);
        delete_fts_by_ids(&tx, &ids)?;
        tx.execute("DELETE FROM nodes WHERE source_name = ?1", params![name])?;
        tx.execute("DELETE FROM sources WHERE name = ?1", params![name])?;

        tx.commit()?;
        Ok(())
    }

    /// Record the provenance of a source: how it was ingested and a cheap
    /// fingerprint to detect upstream changes later. Called after upsert_source.
    pub fn set_source_meta(
        &self,
        source_name: &str,
        source_kind: &str,
        origin: Option<&str>,
        fingerprint: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sources SET source_kind = ?1, origin = ?2, fingerprint = ?3 WHERE name = ?4",
            params![source_kind, origin, fingerprint, source_name],
        )?;
        Ok(())
    }

    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![key],
            |row| row.get(0),
        );
        match result {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.version, s.language, s.ingested_at,
                    s.source_kind, s.origin, s.fingerprint,
                    COUNT(n.id) as node_count
             FROM sources s
             LEFT JOIN nodes n ON n.source_name = s.name
             GROUP BY s.name
             ORDER BY s.name",
        )?;

        let records = stmt
            .query_map([], |row| {
                Ok(SourceRecord {
                    name: row.get(0)?,
                    version: row.get(1)?,
                    language: row.get(2)?,
                    ingested_at: row.get(3)?,
                    source_kind: row.get(4)?,
                    origin: row.get(5)?,
                    fingerprint: row.get(6)?,
                    node_count: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(records)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Default fusion method, overridable via `ROUX_FUSION=rrf` for A/B testing.
/// RRF (additive over ranks) lets a purely graph-reachable node — zero BM25,
/// e.g. one bridged in via a doc `references` edge — still surface, which the
/// multiplicative ScoreFusion (bm25^α × ppr^β) zeroes out.
fn fusion_from_env() -> super::rank::FusionMethod {
    match std::env::var("ROUX_FUSION").as_deref() {
        Ok("rrf") => super::rank::FusionMethod::RRF,
        _ => super::rank::FusionMethod::ScoreFusion,
    }
}

/// Delete every fts_nodes row whose id is in `ids`. (Diagnostic B: tx.execute,
/// matching the pre-fix code.)
fn delete_fts_by_ids(tx: &rusqlite::Transaction<'_>, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    for id in ids {
        tx.execute("DELETE FROM fts_nodes WHERE id = ?1", params![id])?;
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct SearchResult {
    pub matched_ids: Vec<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// PPR scores per node ID (higher = more structurally relevant to the query)
    pub scores: HashMap<String, f64>,
}

use std::collections::HashMap;

#[derive(Clone)]
pub struct SourceRecord {
    pub name: String,
    pub version: String,
    pub language: String,
    pub ingested_at: i64,
    pub source_kind: String,
    pub origin: Option<String>,
    pub fingerprint: Option<String>,
    pub node_count: usize,
}

/// Extract only the meaningful symbol names from a generated description.
/// Strips template words (function, calls, in, class, etc.) and keeps symbol names.
fn extract_description_keywords(desc: &str) -> String {
    const DESC_STOPWORDS: &[&str] = &[
        "function",
        "method",
        "class",
        "struct",
        "enum",
        "trait",
        "impl",
        "module",
        "interface",
        "const",
        "type",
        "file",
        "in",
        "calls",
        "called",
        "by",
        "uses",
        "implements",
        "extends",
        "decorated",
        "with",
        "tested",
        "and",
        "the",
        "a",
        "an",
        "of",
        "for",
        "to",
        "from",
        "is",
        "are",
    ];

    desc.split([',', ' '])
        .map(|w| w.trim())
        .filter(|w| {
            !w.is_empty()
                && w.len() > 2
                && !DESC_STOPWORDS.contains(w)
                && !w.ends_with(".rs")
                && !w.ends_with(".py")
                && !w.ends_with(".js")
                && !w.ends_with(".ts")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Natural-language filler words that pollute lexical search. A query like
/// "how does line buffering work during search" should match on
/// line/buffer/search, not on the generic symbols that "how/does/work" hit.
/// Deliberately conservative: only words that are never meaningful code terms.
const QUERY_STOPWORDS: &[&str] = &[
    "how", "does", "do", "did", "what", "when", "where", "why", "which", "who", "work", "works",
    "working", "during", "the", "a", "an", "of", "for", "to", "from", "is", "are", "be", "this",
    "that", "these", "those", "with", "and", "or", "into", "its", "it", "as", "at", "on", "in",
    "by", "use", "using", "used", "via", "should", "would", "could", "can", "will",
];

/// Cheap morphological stem variants so NL phrasing matches indexed identifiers:
/// "buffering"→"buffer" (LineBuffer), "results"→"result", "mapped"→"map".
/// Returned as ADDITIONAL OR-terms — extra variants only widen recall; PPR and
/// the description rerank handle precision. Garbage stems (rare) match nothing.
pub(crate) fn stem_variants(t: &str) -> Vec<String> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<String>, s: String| {
        if s.len() > 2 && s != t {
            out.push(s);
        }
    };
    if let Some(b) = t.strip_suffix("ing") {
        push(&mut out, b.to_string());
    } else if let Some(b) = t.strip_suffix("ed") {
        // "mapped" → "mapp" → strip the doubled consonant → "map"
        let chars: Vec<char> = b.chars().collect();
        if chars.len() >= 2 && chars[chars.len() - 1] == chars[chars.len() - 2] {
            push(&mut out, b[..b.len() - 1].to_string());
        } else {
            push(&mut out, b.to_string());
        }
    } else if let Some(b) = t.strip_suffix('s')
        && !b.ends_with('s')
    {
        push(&mut out, b.to_string());
    }
    out
}

fn fts_query_escape(query: &str) -> String {
    // Query-side NLP. Stem variants let NL phrasing reach indexed identifiers
    // ("buffering"→"buffer"→LineBuffer) — measured a clean win (ripgrep NL MRR
    // 0.599→0.760, Hit@10 88%→100%) with no CI-gate regression, so ON by default.
    // Stopword removal measured HARMFUL (starves the PPR seed set / desc rerank),
    // so OFF by default; kept behind a flag for future list refinement.
    //   ROUX_QUERY_STEM=0  disable stem variants
    //   ROUX_QUERY_STOP=1  enable stopword removal (experimental)
    //   ROUX_QUERY_NLP=0   master off-switch (disables both)
    let nlp = std::env::var("ROUX_QUERY_NLP")
        .map(|v| v != "0")
        .unwrap_or(true);
    let do_stop = nlp
        && std::env::var("ROUX_QUERY_STOP")
            .map(|v| v == "1")
            .unwrap_or(false);
    let do_stem = nlp
        && std::env::var("ROUX_QUERY_STEM")
            .map(|v| v != "0")
            .unwrap_or(true);
    let mut tokens: Vec<String> = Vec::new();

    // Split on the same separators tokenize_for_fts uses at index time. Without
    // this, queries like "tokio::spawn" collapse to the single token
    // "tokiospawn" — which never matches anything in the index, since the
    // index stored ["tokio", "spawn"].
    for word in query.split_whitespace() {
        for part in word.split([':', '.', '/', '(', ')']) {
            let clean: String = part
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if clean.is_empty() {
                continue;
            }

            let lower = clean.to_lowercase();
            if do_stop && QUERY_STOPWORDS.contains(&lower.as_str()) {
                continue;
            }

            tokens.push(lower.clone());

            // Add subword splits (camelCase/snake_case)
            let subwords = code_tokenize(&clean);
            for sw in &subwords {
                if *sw != lower {
                    tokens.push(sw.clone());
                }
            }

            // Stem variants so NL phrasing reaches indexed identifiers.
            if do_stem {
                for stem in stem_variants(&lower) {
                    tokens.push(stem);
                }
            }
        }
    }

    tokens.sort();
    tokens.dedup();

    if tokens.is_empty() {
        return String::new();
    }

    tokens
        .iter()
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Split a code identifier into subwords.
/// Handles camelCase, PascalCase, snake_case, and SCREAMING_CASE.
/// "parseHTMLDocument" → ["parse", "HTML", "Document"]
/// "fts_query_escape" → ["fts", "query", "escape"]
/// "GraphStore" → ["Graph", "Store"]
pub fn code_tokenize(s: &str) -> Vec<String> {
    if s.is_empty() {
        return vec![];
    }

    let mut tokens = Vec::new();
    let mut current = String::new();

    // First split on underscores
    for part in s.split('_') {
        if part.is_empty() {
            continue;
        }

        // Then split camelCase/PascalCase
        let chars: Vec<char> = part.chars().collect();
        for i in 0..chars.len() {
            let c = chars[i];
            if i > 0 && c.is_uppercase() {
                // Check if this is a transition: lowercase→uppercase or uppercase→uppercase+lowercase
                let prev_lower = chars[i - 1].is_lowercase();
                let next_lower = i + 1 < chars.len() && chars[i + 1].is_lowercase();

                if (prev_lower || (chars[i - 1].is_uppercase() && next_lower))
                    && !current.is_empty()
                {
                    tokens.push(current.clone());
                    current.clear();
                }
            }
            current.push(c);
        }
        if !current.is_empty() {
            tokens.push(current.clone());
            current.clear();
        }
    }

    // Also include the original unsplit form for exact matching
    let original = s.to_string();
    if !tokens.contains(&original) && tokens.len() > 1 {
        tokens.push(original);
    }

    // Lowercase all tokens for case-insensitive matching
    tokens.iter().map(|t| t.to_lowercase()).collect()
}

/// Tokenize text for FTS indexing — splits code identifiers into subwords.
pub fn tokenize_for_fts(text: &str) -> String {
    text.split_whitespace()
        .flat_map(|word| {
            // Split on common code separators
            word.split([':', '.', '/', '(', ')'])
                .flat_map(|part| {
                    let mut tokens = code_tokenize(part);
                    // Emit concatenated form: "walk_dir" → also index "walkdir"
                    let concat: String = part.to_lowercase().replace('_', "");
                    if !tokens.contains(&concat) && concat.len() > 1 {
                        tokens.push(concat);
                    }
                    tokens
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_node(name: &str, kind: &str, qualified: &str) -> Node {
        let id = Node::id_for("test", qualified);
        Node {
            id,
            kind: kind.to_string(),
            name: name.to_string(),
            qualified_name: qualified.to_string(),
            source_name: "test".to_string(),
            language: "rust".to_string(),
            file_path: "src/lib.rs".to_string(),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            visibility: "pub".to_string(),
            signature: Some(format!("fn {name}()")),
            doc: Some(format!("Does {name} things.")),
            body: format!("function: {qualified}\nfn {name}()\nDoes {name} things."),
            parent_id: None,
            content_hash: None,
            line_count: 10,
            source_url: None,
            description: None,
        }
    }

    #[test]
    fn test_upsert_and_search() {
        let store = GraphStore::open_in_memory().unwrap();
        let node = make_node("spawn", "function", "tokio::spawn");
        store
            .upsert_source("test", "1.0.0", "rust", &[node], &[])
            .unwrap();

        let result = store.search("spawn", 5).unwrap();
        assert_eq!(result.matched_ids.len(), 1);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].name, "spawn");
    }

    #[test]
    fn test_fts_delete_removes_all_rows_with_id() {
        // Real-DB behavior check: if fts_nodes already has multiple rows for
        // the same id (legacy duplication), does a single DELETE clear them
        // all? Confirms our cleanup actually heals stale data.
        let store = GraphStore::open_in_memory().unwrap();
        let n = make_node("x", "function", "lib::x");
        store
            .upsert_source("test", "1", "rust", std::slice::from_ref(&n), &[])
            .unwrap();
        // Inject extra duplicate rows to simulate stale FTS rows.
        store
            .conn
            .execute(
                "INSERT INTO fts_nodes(id, name, qualified_name, file_path, signature, doc, body)
                 VALUES (?1, 'x', 'lib::x', 'src/lib.rs', '', '', '')",
                params![n.id],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO fts_nodes(id, name, qualified_name, file_path, signature, doc, body)
                 VALUES (?1, 'x', 'lib::x', 'src/lib.rs', '', '', '')",
                params![n.id],
            )
            .unwrap();
        let before: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM fts_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 3, "test setup: expected 3 fts rows");

        // Now upsert again — sweep should eliminate all duplicates.
        store
            .upsert_source("test", "1", "rust", std::slice::from_ref(&n), &[])
            .unwrap();
        let after: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM fts_nodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, 1,
            "expected fts to be 1:1 after re-upsert; got {after}"
        );
    }

    #[test]
    fn test_upsert_keeps_fts_one_to_one_with_nodes() {
        // Regression: the FTS table accumulated duplicate rows when a source
        // was re-ingested multiple times in the same database. matched_ids
        // came back with each id repeated and PPR seeds got double-weighted.
        // After the fix, fts_nodes is always 1:1 with nodes.
        let store = GraphStore::open_in_memory().unwrap();
        let mk = |qn: &str| Node {
            id: Node::id_for("lib", qn),
            source_name: "lib".into(),
            qualified_name: qn.into(),
            ..make_node(qn.rsplit("::").next().unwrap_or(qn), "function", qn)
        };
        let n1 = mk("lib::alpha");
        let n2 = mk("lib::beta");

        // Three rounds of re-ingestion of the same source — simulates `roux init`
        // followed by repeated `roux sync` runs.
        for _ in 0..3 {
            store
                .upsert_source("lib", "1.0", "rust", &[n1.clone(), n2.clone()], &[])
                .unwrap();
        }

        let fts_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM fts_nodes", [], |row| row.get(0))
            .unwrap();
        let nodes_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            fts_count, nodes_count,
            "fts_nodes ({fts_count}) should be 1:1 with nodes ({nodes_count})"
        );

        // And matched_ids in a search result should not contain duplicates.
        let result = store.search("alpha", 5).unwrap();
        let unique: std::collections::HashSet<_> = result.matched_ids.iter().collect();
        assert_eq!(
            unique.len(),
            result.matched_ids.len(),
            "matched_ids contains duplicates: {:?}",
            result.matched_ids
        );

        // Re-ingesting with a smaller node set must drop the removed symbol's
        // FTS row (otherwise stale results haunt searches). beta should be gone.
        store
            .upsert_source("lib", "1.0", "rust", std::slice::from_ref(&n1), &[])
            .unwrap();
        let beta_hits = store.search("beta", 5).unwrap();
        assert!(
            beta_hits.matched_ids.is_empty(),
            "expected no hits for removed symbol, got {:?}",
            beta_hits.matched_ids
        );

        // remove_source must drop everything for that source.
        store.remove_source("lib").unwrap();
        let any: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM fts_nodes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(any, 0, "remove_source left {any} fts rows behind");
    }

    #[test]
    fn test_search_scoped_filters_by_source() {
        let store = GraphStore::open_in_memory().unwrap();

        // Two sources each have a node named `shared_thing` — BM25 matches both
        // but the scoped search must only surface one source's results.
        let n_alpha = {
            let qn = "alpha::shared_thing";
            Node {
                id: Node::id_for("alpha", qn),
                source_name: "alpha".into(),
                qualified_name: qn.into(),
                ..make_node("shared_thing", "function", qn)
            }
        };
        let n_beta = {
            let qn = "beta::shared_thing";
            Node {
                id: Node::id_for("beta", qn),
                source_name: "beta".into(),
                qualified_name: qn.into(),
                ..make_node("shared_thing", "function", qn)
            }
        };
        store
            .upsert_source("alpha", "1.0", "rust", &[n_alpha], &[])
            .unwrap();
        store
            .upsert_source("beta", "1.0", "rust", &[n_beta], &[])
            .unwrap();

        // Unfiltered: both sources' nodes are returned.
        let all = store.search_scoped("shared_thing", 10, None).unwrap();
        assert_eq!(all.nodes.len(), 2);

        // Scoped to alpha: only alpha's node.
        let only_alpha = store
            .search_scoped("shared_thing", 10, Some("alpha"))
            .unwrap();
        assert_eq!(only_alpha.nodes.len(), 1);
        assert_eq!(only_alpha.nodes[0].source_name, "alpha");
    }

    #[test]
    fn test_search_multi_fuses_variants() {
        let store = GraphStore::open_in_memory().unwrap();
        // Two distinct symbols, each reachable only by its own jargon term —
        // the agent-reformulation scenario.
        let ringing = make_node("InputShaping", "function", "s::InputShaping");
        let jerk = make_node("JunctionDeviation", "function", "s::JunctionDeviation");
        store
            .upsert_source("s", "1.0", "rust", &[ringing, jerk], &[])
            .unwrap();

        // Each single query finds only its own symbol.
        let a = store.search_scoped("InputShaping", 10, None).unwrap();
        assert_eq!(a.matched_ids.len(), 1);
        let b = store.search_scoped("JunctionDeviation", 10, None).unwrap();
        assert_eq!(b.matched_ids.len(), 1);

        // Fused: one call surfaces BOTH — the union a single query can't reach.
        let fused = store
            .search_multi(
                &["InputShaping".into(), "JunctionDeviation".into()],
                10,
                None,
            )
            .unwrap();
        let names: Vec<&str> = fused.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"InputShaping"), "got {names:?}");
        assert!(names.contains(&"JunctionDeviation"), "got {names:?}");

        // A single-element multi search is equivalent to a plain scoped search.
        let single = store.search_multi(&["InputShaping".into()], 10, None).unwrap();
        assert_eq!(single.matched_ids, a.matched_ids);

        // Empty input is a no-op, not an error.
        assert!(store.search_multi(&[], 10, None).unwrap().nodes.is_empty());
    }

    #[test]
    fn test_search_scoped_missing_source_errors() {
        let store = GraphStore::open_in_memory().unwrap();
        let n = make_node("x", "function", "s::x");
        store.upsert_source("s", "1.0", "rust", &[n], &[]).unwrap();

        let err = store
            .search_scoped("x", 10, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"));
        assert!(err.contains("roux list"));
    }

    #[test]
    fn test_graph_expansion() {
        let store = GraphStore::open_in_memory().unwrap();

        let auth = make_node("authenticate", "function", "auth::authenticate");
        let validate = make_node("validate_token", "function", "auth::validate_token");
        let hash = make_node("hash_password", "function", "auth::hash_password");

        let edges = vec![
            Edge {
                from_id: auth.id.clone(),
                to_id: validate.id.clone(),
                kind: "calls".to_string(),
            },
            Edge {
                from_id: auth.id.clone(),
                to_id: hash.id.clone(),
                kind: "calls".to_string(),
            },
        ];

        store
            .upsert_source("test", "1.0.0", "rust", &[auth, validate, hash], &edges)
            .unwrap();

        let result = store.search("authenticate", 5).unwrap();
        assert_eq!(result.matched_ids.len(), 1);
        assert_eq!(result.nodes.len(), 3, "should expand to called nodes");
        assert_eq!(result.edges.len(), 2);
    }

    #[test]
    fn test_parent_expansion() {
        let store = GraphStore::open_in_memory().unwrap();

        let file_node = Node {
            id: Node::id_for("test", "test::src/auth.rs"),
            kind: "file".to_string(),
            name: "auth.rs".to_string(),
            qualified_name: "test::src/auth.rs".to_string(),
            source_name: "test".to_string(),
            language: "rust".to_string(),
            file_path: "src/auth.rs".to_string(),
            start_line: 0,
            start_col: 0,
            end_line: 0,
            visibility: String::new(),
            signature: None,
            doc: None,
            body: "file: src/auth.rs".to_string(),
            parent_id: None,
            content_hash: None,
            line_count: 0,
            source_url: None,
            description: None,
        };

        let mut func = make_node("login", "function", "test::login");
        func.parent_id = Some(file_node.id.clone());

        store
            .upsert_source("test", "1.0.0", "rust", &[file_node, func], &[])
            .unwrap();

        let result = store.search("login", 5).unwrap();
        assert_eq!(result.matched_ids.len(), 1);
        assert!(
            result.nodes.len() >= 2,
            "should expand to include parent file node"
        );
    }

    #[test]
    fn test_children_expansion() {
        let store = GraphStore::open_in_memory().unwrap();

        let class = make_node("MyClass", "class", "test::MyClass");
        let mut method = make_node("do_thing", "method", "test::MyClass::do_thing");
        method.parent_id = Some(class.id.clone());

        store
            .upsert_source("test", "1.0.0", "rust", &[class, method], &[])
            .unwrap();

        // Search for class — should also return its methods
        let result = store.search("MyClass", 5).unwrap();
        assert_eq!(
            result.nodes.len(),
            2,
            "should expand to include child method"
        );
    }

    #[test]
    fn test_remove_source() {
        let store = GraphStore::open_in_memory().unwrap();
        let node = make_node("foo", "function", "lib::foo");
        store
            .upsert_source("test", "1.0.0", "rust", &[node], &[])
            .unwrap();

        store.remove_source("test").unwrap();
        let result = store.search("foo", 5).unwrap();
        assert!(result.nodes.is_empty());
    }

    #[test]
    fn test_list_sources() {
        let store = GraphStore::open_in_memory().unwrap();
        let mut nodes = vec![
            make_node("foo", "function", "mylib::foo"),
            make_node("bar", "function", "mylib::bar"),
        ];
        for n in &mut nodes {
            n.source_name = "mylib".to_string();
        }
        store
            .upsert_source("mylib", "2.0.0", "rust", &nodes, &[])
            .unwrap();

        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "mylib");
        assert_eq!(sources[0].node_count, 2);
    }

    #[test]
    fn test_empty_search() {
        let store = GraphStore::open_in_memory().unwrap();
        let result = store.search("nonexistent", 5).unwrap();
        assert!(result.nodes.is_empty());
    }

    #[test]
    fn test_schema_version() {
        let store = GraphStore::open_in_memory().unwrap();
        let version: String = store
            .conn
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "7");
    }

    #[test]
    fn test_fts_walkdir() {
        // Verify index-time concatenation: "walk_dir" should be findable as "walkdir"
        let fts = tokenize_for_fts("walk_dir");
        eprintln!("tokenize_for_fts('walk_dir') = '{fts}'");
        assert!(
            fts.contains("walkdir"),
            "should contain concatenated form, got: {fts}"
        );

        let store = GraphStore::open_in_memory().unwrap();
        let node = make_node("walk_dir", "function", "test::walk_dir");
        store
            .upsert_source("test", "dev", "rust", &[node], &[])
            .unwrap();

        let r1 = store.search("walkdir", 10).unwrap();
        assert!(
            r1.nodes.iter().any(|n| n.name == "walk_dir"),
            "walk_dir should be findable as 'walkdir'"
        );
    }

    #[test]
    fn test_diff_source() {
        let store = GraphStore::open_in_memory().unwrap();

        let mut n1 = make_node("foo", "function", "test::foo");
        n1.content_hash = Some("hash_a".to_string());
        let mut n2 = make_node("bar", "function", "test::bar");
        n2.content_hash = Some("hash_b".to_string());

        store
            .upsert_source("test", "dev", "rust", &[n1, n2], &[])
            .unwrap();

        // New extraction: foo unchanged, bar modified, baz added
        let mut new_n1 = make_node("foo", "function", "test::foo");
        new_n1.content_hash = Some("hash_a".to_string());
        let mut new_n2 = make_node("bar", "function", "test::bar");
        new_n2.content_hash = Some("hash_b_changed".to_string());
        let new_n3 = make_node("baz", "function", "test::baz");

        let (added, modified, removed) = store
            .diff_source("test", &[new_n1, new_n2, new_n3])
            .unwrap();

        assert_eq!(added.len(), 1, "baz should be added");
        assert_eq!(modified.len(), 1, "bar should be modified");
        assert!(removed.is_empty(), "nothing removed");

        // Re-extract without n1 — it should show as removed
        let mut only_n2 = make_node("bar", "function", "test::bar");
        only_n2.content_hash = Some("hash_b".to_string());
        let (_, _, removed2) = store.diff_source("test", &[only_n2]).unwrap();
        assert_eq!(removed2.len(), 1, "foo should be removed");
    }
}
