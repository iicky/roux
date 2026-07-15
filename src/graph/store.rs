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
            // Keep the local index out of the user's git status: a self-ignoring
            // `.roux/.gitignore` covers the whole dir without touching the repo's
            // root .gitignore. Only for the local `.roux` dir, and only if absent.
            if parent.file_name().is_some_and(|n| n == ".roux") {
                let ignore = parent.join(".gitignore");
                if !ignore.exists() {
                    std::fs::write(&ignore, "*\n").ok();
                }
            }
        }

        let conn = Connection::open(path)
            .with_context(|| format!("opening database at {}", path.display()))?;

        // busy_timeout so concurrent writers (two `roux add`/`init`, or the MCP
        // server + a CLI call) wait for the lock instead of failing immediately
        // with SQLITE_BUSY.
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;

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
                    ref_name TEXT,
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

        if version < 8 {
            // Per-file manifest: lets an incremental refresh compute
            // the changed-file set (added/modified/deleted) without re-parsing
            // unchanged files, and lets a query detect staleness vs the working
            // tree. content_hash is the authoritative change key; mtime is a
            // cheap pre-filter. Existing indexes gain an empty manifest until
            // their next `roux add`/`init`.
            self.conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS files (
                    source_name  TEXT NOT NULL,
                    path         TEXT NOT NULL,
                    content_hash TEXT NOT NULL,
                    mtime        INTEGER,
                    indexed_at   INTEGER NOT NULL,
                    PRIMARY KEY (source_name, path)
                );
                CREATE INDEX IF NOT EXISTS idx_files_source ON files(source_name);
                INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '8');",
            )?;
        }

        if version < 9 {
            // Persist the raw reference token per edge so reference resolution can
            // be re-run over stored data during an incremental refresh (after a
            // target symbol is renamed/added/removed). Ignore "duplicate column"
            // so a freshly-created DB (column already in CREATE TABLE) is fine.
            let _ = self
                .conn
                .execute("ALTER TABLE edges ADD COLUMN ref_name TEXT", []);
            self.conn.execute(
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '9')",
                [],
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

        // Remove old data for this source. See delete_fts_by_ids for why FTS
        // deletion is batched.
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
                insert_node_and_fts(&mut node_stmt, &mut fts_stmt, node)?;
            }
        }

        // Insert edges
        {
            let mut edge_stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO edges (from_id, to_id, kind, ref_name) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for edge in edges {
                edge_stmt.execute(params![edge.from_id, edge.to_id, edge.kind, edge.ref_name])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Apply a re-extracted graph as a minimal delta against what is stored for
    /// `source_name`, touching only rows that actually changed. Unlike
    /// [`GraphStore::upsert_source`], which wipes and reinserts the whole
    /// source, this updates only added/modified/removed nodes (and their FTS
    /// rows) and reconciles the source's edges, leaving unchanged rows in place.
    /// The resulting stored graph is byte-identical to a full `upsert_source` of
    /// the same `nodes`/`edges`.
    ///
    /// A node counts as modified when ANY persisted field differs — not merely
    /// its `content_hash` — because a global re-resolve/description pass can
    /// change an unchanged-text symbol's edges, line span, or description.
    /// Edges are reconciled by full identity `(from_id, to_id, kind, ref_name)`,
    /// scoped to the source's stored node ids on EITHER endpoint (mirroring
    /// `remove_source`), so a removed node leaves no dangling incoming edge.
    pub fn apply_source_delta(
        &self,
        source_name: &str,
        source_version: &str,
        language: &str,
        nodes: &[Node],
        edges: &[Edge],
    ) -> Result<DeltaStats> {
        use std::collections::HashSet;

        let tx = self.conn.unchecked_transaction()?;

        // Read current state for this source within the transaction snapshot.
        let stored_nodes: Vec<Node> = {
            let mut stmt = tx.prepare(
                "SELECT id, kind, name, qualified_name, source_name, language,
                        file_path, start_line, start_col, end_line, visibility,
                        signature, doc, body, parent_id, content_hash, line_count, source_url, description
                 FROM nodes WHERE source_name = ?1",
            )?;
            stmt.query_map(params![source_name], row_to_node)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        // Edges touching this source on EITHER endpoint — the set a full rebuild
        // (remove_source + upsert_source) would drop before reinserting.
        let stored_edges: Vec<Edge> = {
            let mut stmt = tx.prepare(
                "SELECT from_id, to_id, kind, ref_name FROM edges
                 WHERE from_id IN (SELECT id FROM nodes WHERE source_name = ?1)
                    OR to_id   IN (SELECT id FROM nodes WHERE source_name = ?1)",
            )?;
            stmt.query_map(params![source_name], |row| {
                Ok(Edge {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    kind: row.get(2)?,
                    ref_name: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        // Node delta by FULL record equality (content_hash alone is insufficient).
        let stored_by_id: HashMap<&str, &Node> =
            stored_nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let new_ids: HashSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();

        let mut added = 0usize;
        let mut modified = 0usize;
        let mut upsert_ids: Vec<String> = Vec::new();
        let mut upserts: Vec<&Node> = Vec::new();
        for node in nodes {
            match stored_by_id.get(node.id.as_str()) {
                None => {
                    added += 1;
                    upsert_ids.push(node.id.clone());
                    upserts.push(node);
                }
                Some(&stored) if stored != node => {
                    modified += 1;
                    upsert_ids.push(node.id.clone());
                    upserts.push(node);
                }
                Some(_) => {}
            }
        }
        let removed_ids: Vec<String> = stored_nodes
            .iter()
            .filter(|n| !new_ids.contains(n.id.as_str()))
            .map(|n| n.id.clone())
            .collect();

        // Edge delta by full identity (a re-resolved to_id is a delete + insert).
        let edge_key = |e: &Edge| {
            (
                e.from_id.clone(),
                e.to_id.clone(),
                e.kind.clone(),
                e.ref_name.clone(),
            )
        };
        let stored_edge_set: HashSet<_> = stored_edges.iter().map(edge_key).collect();
        let new_edge_set: HashSet<_> = edges.iter().map(edge_key).collect();
        let edges_to_delete: Vec<&Edge> = stored_edges
            .iter()
            .filter(|e| !new_edge_set.contains(&edge_key(e)))
            .collect();
        let edges_to_insert: Vec<&Edge> = edges
            .iter()
            .filter(|e| !stored_edge_set.contains(&edge_key(e)))
            .collect();

        // Source record (version/language/timestamp), as upsert_source does.
        tx.execute(
            "INSERT OR REPLACE INTO sources (name, version, language, ingested_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![source_name, source_version, language, now()],
        )?;

        // Removed nodes: FTS rows then node rows. Their edges are dropped by the
        // edge reconciliation below (present in stored_edges, absent from new).
        delete_fts_by_ids(&tx, &removed_ids)?;
        {
            let mut del = tx.prepare_cached("DELETE FROM nodes WHERE id = ?1")?;
            for id in &removed_ids {
                del.execute(params![id])?;
            }
        }

        // Added + modified nodes: drop any stale FTS row, then (re)insert node+FTS.
        delete_fts_by_ids(&tx, &upsert_ids)?;
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
            for node in upserts {
                insert_node_and_fts(&mut node_stmt, &mut fts_stmt, node)?;
            }
        }

        // Edges: delete stale, insert new; matched edges are left untouched.
        {
            let mut edge_del = tx.prepare_cached(
                "DELETE FROM edges WHERE from_id = ?1 AND to_id = ?2 AND kind = ?3",
            )?;
            for e in &edges_to_delete {
                edge_del.execute(params![e.from_id, e.to_id, e.kind])?;
            }
        }
        {
            let mut edge_ins = tx.prepare_cached(
                "INSERT OR REPLACE INTO edges (from_id, to_id, kind, ref_name) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in &edges_to_insert {
                edge_ins.execute(params![e.from_id, e.to_id, e.kind, e.ref_name])?;
            }
        }

        tx.commit()?;

        Ok(DeltaStats {
            nodes_added: added,
            nodes_modified: modified,
            nodes_removed: removed_ids.len(),
            edges_added: edges_to_insert.len(),
            edges_removed: edges_to_delete.len(),
        })
    }

    /// Search by keyword, return matching nodes + their graph neighborhood.
    pub fn search(&self, query: &str, limit: usize) -> Result<SearchResult> {
        self.search_with_opts(query, limit, super::rank::FusionMethod::ScoreFusion, None)
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
        self.search_with_opts(query, limit, super::rank::FusionMethod::ScoreFusion, source)
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
        let cfg = crate::settings::get();
        let rrf_k = cfg.rrf_k;
        let per_query = limit.saturating_mul(cfg.candidate_multiplier).max(limit);
        let mut rrf: HashMap<String, f64> = HashMap::new();
        let mut node_map: HashMap<String, Node> = HashMap::new();
        let mut edge_set: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        let mut edges: Vec<Edge> = Vec::new();

        for q in &queries {
            let res = self.search_scoped(q, per_query, source)?;
            for (rank, id) in res.matched_ids.iter().enumerate() {
                *rrf.entry(id.clone()).or_insert(0.0) += 1.0 / (rrf_k + (rank + 1) as f64);
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
        let matched_ids: Vec<String> = fused.iter().take(limit).map(|(id, _)| id.clone()).collect();
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
        source: Option<&str>,
    ) -> Result<SearchResult> {
        let safe_query = fts_query_escape(query);
        if safe_query.is_empty() {
            return Ok(SearchResult::default());
        }

        let cfg = crate::settings::get();
        // Over-fetch BM25 candidates so graph re-ranking has room to promote
        // neighbors over raw lexical hits.
        let candidate_limit = limit.saturating_mul(cfg.candidate_multiplier) as i64;

        // BM25 search on FTS index — capture scores. When a source filter is
        // set, join against nodes to restrict matches to that source.
        let bm25_results: Vec<(String, f64)> = if let Some(src) = source {
            let mut stmt = self.conn.prepare(
                "SELECT f.id, f.rank FROM fts_nodes f
                 JOIN nodes n ON n.id = f.id
                 WHERE fts_nodes MATCH ?1 AND n.source_name = ?2
                 ORDER BY f.rank LIMIT ?3",
            )?;
            stmt.query_map(params![safe_query, src, candidate_limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            let mut stmt = self.conn.prepare(
                "SELECT id, rank FROM fts_nodes WHERE fts_nodes MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            stmt.query_map(params![safe_query, candidate_limit], |row| {
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

        // Pull a 2-hop ego-graph around the seed nodes via BFS. Expansion is
        // bounded so a hub symbol (a widely-called function, or a module that
        // "contains" hundreds of children) can't drag the whole graph into the
        // working set and blow up the downstream PPR pass. Two guards, both
        // tunable via `ROUX_*` env (see the `settings` module):
        //   * max_node_degree — skip expanding *through* an ultra-high-degree
        //     node. Hubs connect to everything, so their neighbors are low-signal
        //     for ranking while enormously inflating the set. The hub itself
        //     still stays in the graph; only its neighbors are skipped.
        //   * max_subgraph_nodes — hard backstop on total working-set size so
        //     PPR cost stays bounded regardless of seed count or graph shape.
        let max_node_degree = cfg.max_node_degree;
        let max_subgraph_nodes = cfg.max_subgraph_nodes;

        // Pull one past the cap so a node sitting exactly at the threshold is
        // kept while a genuine hub (strictly more) is detected and skipped.
        let degree_probe = (max_node_degree + 1) as i64;
        let mut seen: std::collections::HashSet<String> = matched_ids.iter().cloned().collect();
        let mut frontier: Vec<String> = matched_ids.clone();

        // Hop 1 + Hop 2: expand edges + parent/children from the frontier only.
        for _hop in 0..2 {
            if seen.len() >= max_subgraph_nodes {
                break;
            }
            let mut next_frontier = Vec::new();
            for id in &frontier {
                let mut candidates: Vec<String> = Vec::new();

                // Edge neighbors (both directions), capped to detect hubs.
                let mut stmt = self.conn.prepare_cached(
                    "SELECT to_id FROM edges
                     WHERE from_id = ?1 AND to_id NOT LIKE '__unresolved::%' AND to_id NOT LIKE '__route::%'
                     UNION
                     SELECT from_id FROM edges
                     WHERE to_id = ?1 AND from_id NOT LIKE '__unresolved::%' AND from_id NOT LIKE '__route::%'
                     LIMIT ?2",
                )?;
                let neighbors: Vec<String> = stmt
                    .query_map(params![id, degree_probe], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if neighbors.len() <= max_node_degree {
                    candidates.extend(neighbors);
                }

                // Parent (at most one)
                if let Ok(pid) = self.conn.query_row(
                    "SELECT parent_id FROM nodes WHERE id = ?1 AND parent_id IS NOT NULL",
                    params![id],
                    |row| row.get::<_, String>(0),
                ) {
                    candidates.push(pid);
                }

                // Children, capped the same way so a giant container doesn't flood.
                let mut stmt = self
                    .conn
                    .prepare_cached("SELECT id FROM nodes WHERE parent_id = ?1 LIMIT ?2")?;
                let children: Vec<String> = stmt
                    .query_map(params![id, degree_probe], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if children.len() <= max_node_degree {
                    candidates.extend(children);
                }

                for c in candidates {
                    if seen.len() >= max_subgraph_nodes {
                        break;
                    }
                    if seen.insert(c.clone()) {
                        next_frontier.push(c);
                    }
                }
                if seen.len() >= max_subgraph_nodes {
                    break;
                }
            }
            frontier = next_frontier;
        }

        let mut all_ids: Vec<String> = seen.into_iter().collect();
        all_ids.sort();

        // Fetch full subgraph
        let nodes = self.fetch_nodes(&all_ids)?;
        let edges = self.fetch_edges(&all_ids)?;

        // Run PPR ranking on the subgraph, fused with BM25 scores
        let ranked = super::rank::rank_subgraph_with(
            nodes,
            edges,
            &matched_ids,
            &bm25_scores,
            limit,
            fusion,
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
                 WHERE (from_id IN ({}) OR to_id IN ({}))
                   AND from_id NOT LIKE '__unresolved::%' AND to_id NOT LIKE '__unresolved::%'
                   AND from_id NOT LIKE '__route::%' AND to_id NOT LIKE '__route::%'",
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
                        ref_name: None,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all_edges.extend(edges);
        }

        Ok(all_edges)
    }

    /// Load every node in the index (for a global re-resolution pass). Ordered
    /// deterministically so repeated resolutions break ambiguous-name ties the
    /// same way every run.
    fn all_nodes(&self) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, name, qualified_name, source_name, language,
                    file_path, start_line, start_col, end_line, visibility,
                    signature, doc, body, parent_id, content_hash, line_count, source_url, description
             FROM nodes ORDER BY file_path, start_line, id",
        )?;
        let nodes = stmt
            .query_map([], row_to_node)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(nodes)
    }

    /// Load every edge WITH its raw ref_name — including retained unresolved
    /// edges (sentinel to_id). Unlike fetch_edges (query-scoped, ref-name-free),
    /// this is the input to a re-resolution pass.
    fn all_edges_with_refs(&self) -> Result<Vec<Edge>> {
        let mut stmt = self
            .conn
            .prepare("SELECT from_id, to_id, kind, ref_name FROM edges")?;
        let edges = stmt
            .query_map([], |row| {
                Ok(Edge {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    kind: row.get(2)?,
                    ref_name: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(edges)
    }

    /// Re-run reference resolution over all stored edges/nodes. Each edge persists
    /// its raw ref_name, so resolution is re-runnable after an incremental update
    /// changes the node set: edges into renamed/moved symbols re-point, and
    /// previously-unresolved edges connect to newly-added targets. Returns the
    /// number of edges whose to_id changed.
    pub fn reresolve(&self) -> Result<usize> {
        let nodes = self.all_nodes()?;
        let mut edges = self.all_edges_with_refs()?;
        let before: Vec<String> = edges.iter().map(|e| e.to_id.clone()).collect();
        super::extract::resolve_references(&mut edges, &nodes);
        let changed = edges
            .iter()
            .zip(&before)
            .filter(|(e, b)| e.to_id != **b)
            .count();
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM edges", [])?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO edges (from_id, to_id, kind, ref_name) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in &edges {
                stmt.execute(params![e.from_id, e.to_id, e.kind, e.ref_name])?;
            }
        }
        tx.commit()?;
        Ok(changed)
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

    /// Replace the per-file manifest for a source. Called alongside
    /// `upsert_source` at index time so a later refresh can compute the
    /// changed-file set without re-parsing.
    pub fn replace_files(
        &self,
        source_name: &str,
        files: &[super::extract::FileMeta],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM files WHERE source_name = ?1",
            params![source_name],
        )?;
        {
            let now = now();
            let mut stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO files
                    (source_name, path, content_hash, mtime, indexed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for f in files {
                stmt.execute(params![source_name, f.path, f.content_hash, f.mtime, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The stored manifest for a source as `path -> content_hash`.
    pub fn stored_files(&self, source_name: &str) -> Result<HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, content_hash FROM files WHERE source_name = ?1")?;
        let map = stmt
            .query_map(params![source_name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(map)
    }

    /// The stored per-file manifest for a source as [`FileMeta`] entries — the
    /// change baseline an incremental refresh diffs the working tree against.
    pub fn source_files(&self, source_name: &str) -> Result<Vec<super::extract::FileMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT path, content_hash, mtime FROM files WHERE source_name = ?1 ORDER BY path",
        )?;
        let files = stmt
            .query_map(params![source_name], |row| {
                Ok(super::extract::FileMeta {
                    path: row.get(0)?,
                    content_hash: row.get(1)?,
                    mtime: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(files)
    }

    /// Reconstruct the stored graph for a source as a [`FileGraph`] — the prior
    /// input to [`super::extract::reextract_incremental`]. Only the node/edge
    /// SETS matter (finalize re-canonicalizes order), but rows are loaded in
    /// canonical order anyway. Edges are scoped to the source's own nodes
    /// (`from_id`), i.e. the edges this source emitted.
    pub fn source_graph(&self, source_name: &str) -> Result<super::extract::FileGraph> {
        let nodes: Vec<Node> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, name, qualified_name, source_name, language,
                        file_path, start_line, start_col, end_line, visibility,
                        signature, doc, body, parent_id, content_hash, line_count, source_url, description
                 FROM nodes WHERE source_name = ?1 ORDER BY file_path, start_line, id",
            )?;
            stmt.query_map(params![source_name], row_to_node)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let edges: Vec<Edge> = {
            let mut stmt = self.conn.prepare(
                "SELECT from_id, to_id, kind, ref_name FROM edges
                 WHERE from_id IN (SELECT id FROM nodes WHERE source_name = ?1)
                 ORDER BY from_id, to_id, kind",
            )?;
            stmt.query_map(params![source_name], |row| {
                Ok(Edge {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    kind: row.get(2)?,
                    ref_name: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let files = self.source_files(source_name)?;
        Ok(super::extract::FileGraph {
            nodes,
            edges,
            files,
        })
    }

    /// Compare a freshly-walked file list against the stored manifest, by
    /// content hash. `current` is typically produced by re-walking the source
    /// tree (cheap: read + hash, no parse).
    pub fn diff_files(
        &self,
        source_name: &str,
        current: &[super::extract::FileMeta],
    ) -> Result<FileDiff> {
        let stored = self.stored_files(source_name)?;
        let current_paths: std::collections::HashSet<&str> =
            current.iter().map(|f| f.path.as_str()).collect();

        let mut added = Vec::new();
        let mut modified = Vec::new();
        for f in current {
            match stored.get(&f.path) {
                None => added.push(f.path.clone()),
                Some(h) if *h != f.content_hash => modified.push(f.path.clone()),
                Some(_) => {}
            }
        }
        let deleted: Vec<String> = stored
            .keys()
            .filter(|p| !current_paths.contains(p.as_str()))
            .cloned()
            .collect();
        Ok(FileDiff {
            added,
            modified,
            deleted,
        })
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
        tx.execute("DELETE FROM files WHERE source_name = ?1", params![name])?;
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

/// Delete every fts_nodes row whose id is in `ids`.
///
/// `id` is an UNINDEXED FTS5 column, so `WHERE id = ?` is a full scan of the FTS
/// content table. Deleting one id at a time is therefore O(nodes × ids) — on a
/// re-index of a 100k-node source that's 100k full scans. Batch into `IN`
/// clauses so each chunk is a single scan (≈O(nodes) total).
fn delete_fts_by_ids(tx: &rusqlite::Transaction<'_>, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // Stay under SQLite's 32766 parameter limit.
    for chunk in ids.chunks(20000) {
        let placeholders: Vec<String> = (1..=chunk.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "DELETE FROM fts_nodes WHERE id IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = chunk
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        tx.execute(&sql, params.as_slice())?;
    }
    Ok(())
}

/// Insert a node row and its FTS row with the exact column layout and FTS
/// tokenization used by [`GraphStore::upsert_source`], so an incremental delta
/// produces rows byte-identical to a full re-insert. Callers MUST have already
/// removed any prior FTS row for `node.id` (FTS5 has no upsert).
fn insert_node_and_fts(
    node_stmt: &mut rusqlite::CachedStatement<'_>,
    fts_stmt: &mut rusqlite::CachedStatement<'_>,
    node: &Node,
) -> rusqlite::Result<()> {
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
    // Index both original text AND tokenized form for best of both.
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
    Ok(())
}

/// Map a `nodes` row (in the canonical 19-column SELECT order) to a [`Node`].
fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
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
}

/// Row-level summary of a [`GraphStore::apply_source_delta`] update: how many
/// node and edge rows were actually written, for verifying an incremental
/// refresh touched only what changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeltaStats {
    pub nodes_added: usize,
    pub nodes_modified: usize,
    pub nodes_removed: usize,
    pub edges_added: usize,
    pub edges_removed: usize,
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

/// Result of comparing a freshly-walked file list against the stored manifest.
/// Paths are relative to the source root.
#[derive(Debug, Default, PartialEq)]
pub struct FileDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl FileDiff {
    /// True when the working tree matches the manifest.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

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
            // Rebuild from chars, not a byte slice: b.len() is bytes, so
            // b[..b.len()-1] panics when the trailing char is multibyte.
            let stripped: String = chars[..chars.len() - 1].iter().collect();
            push(&mut out, stripped);
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
    // Query-side stemming: emit stem variants so natural-language phrasing reaches
    // indexed identifiers ("buffering"→"buffer"→LineBuffer). Measured a clean win
    // (ripgrep NL MRR 0.599→0.760, Hit@10 88%→100%) with no CI-gate regression.
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
            tokens.push(lower.clone());

            // Add subword splits (camelCase/snake_case)
            for sw in &code_tokenize(&clean) {
                if *sw != lower {
                    tokens.push(sw.clone());
                }
            }

            // Stem variants so NL phrasing reaches indexed identifiers.
            for stem in stem_variants(&lower) {
                tokens.push(stem);
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
    use crate::graph::extract::{FileMeta, extract_dir, reextract_incremental};

    fn fmeta(path: &str, hash: &str) -> FileMeta {
        FileMeta {
            path: path.to_string(),
            content_hash: hash.to_string(),
            mtime: Some(1),
        }
    }

    #[test]
    fn file_manifest_round_trips_and_diffs() {
        let store = GraphStore::open_in_memory().unwrap();
        let v1 = [fmeta("src/a.rs", "hA"), fmeta("src/b.rs", "hB")];
        store.replace_files("s", &v1).unwrap();

        // Round-trip.
        let stored = store.stored_files("s").unwrap();
        assert_eq!(stored.get("src/a.rs").map(String::as_str), Some("hA"));
        assert_eq!(stored.len(), 2);

        // No change → empty diff.
        assert!(store.diff_files("s", &v1).unwrap().is_empty());

        // a.rs modified, b.rs deleted, c.rs added.
        let v2 = [fmeta("src/a.rs", "hA2"), fmeta("src/c.rs", "hC")];
        let diff = store.diff_files("s", &v2).unwrap();
        assert_eq!(diff.added, vec!["src/c.rs"]);
        assert_eq!(diff.modified, vec!["src/a.rs"]);
        assert_eq!(diff.deleted, vec!["src/b.rs"]);

        // replace_files fully replaces (no leftover rows from v1).
        store.replace_files("s", &v2).unwrap();
        assert_eq!(store.stored_files("s").unwrap().len(), 2);
        assert!(store.diff_files("s", &v2).unwrap().is_empty());
    }

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
        let single = store
            .search_multi(&["InputShaping".into()], 10, None)
            .unwrap();
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
                ref_name: None,
            },
            Edge {
                from_id: auth.id.clone(),
                to_id: hash.id.clone(),
                kind: "calls".to_string(),
                ref_name: None,
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
        assert_eq!(version, "9");
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

    #[test]
    fn test_hub_expansion_is_capped() {
        // A hub symbol called by hundreds of functions must not drag its entire
        // neighborhood into the working set. Expansion skips *through* nodes
        // above max_node_degree (default 128) — the hub stays, callers don't flood in.
        let store = GraphStore::open_in_memory().unwrap();

        let hub = make_node("dispatch", "function", "lib::dispatch");
        let mut nodes = vec![hub.clone()];
        let mut edges = Vec::new();
        for i in 0..300 {
            let caller = make_node(
                &format!("caller_{i}"),
                "function",
                &format!("lib::caller_{i}"),
            );
            edges.push(Edge {
                from_id: caller.id.clone(),
                to_id: hub.id.clone(),
                kind: "calls".to_string(),
                ref_name: None,
            });
            nodes.push(caller);
        }
        store
            .upsert_source("lib", "1.0", "rust", &nodes, &edges)
            .unwrap();

        // Only the hub matches "dispatch"; its 300 callers do not. Because the
        // hub's degree (300) exceeds the cap, none of the callers are expanded.
        let result = store.search("dispatch", 10).unwrap();
        assert_eq!(
            result.matched_ids,
            vec![hub.id.clone()],
            "hub is the sole seed"
        );
        assert!(
            result.nodes.len() < 128,
            "hub neighborhood should be capped, got {} nodes",
            result.nodes.len()
        );
    }

    #[test]
    fn test_small_neighborhood_still_expands() {
        // Guard against over-capping: a node with a handful of neighbors must
        // still pull them into the subgraph (the cap only bites on hubs).
        let store = GraphStore::open_in_memory().unwrap();

        let seed = make_node("authenticate", "function", "lib::authenticate");
        let callee = make_node("validate_token", "function", "lib::validate_token");
        let edges = vec![Edge {
            from_id: seed.id.clone(),
            to_id: callee.id.clone(),
            kind: "calls".to_string(),
            ref_name: None,
        }];
        store
            .upsert_source(
                "lib",
                "1.0",
                "rust",
                &[seed.clone(), callee.clone()],
                &edges,
            )
            .unwrap();

        let result = store.search("authenticate", 10).unwrap();
        let ids: std::collections::HashSet<&String> = result.nodes.iter().map(|n| &n.id).collect();
        assert!(
            ids.contains(&callee.id),
            "connected neighbor should be expanded in"
        );
    }

    #[test]
    fn resolve_references_retains_unresolved_with_ref_name() {
        // An edge whose target name never resolves must be RETAINED (not
        // dropped) with the raw token captured into ref_name, so a later
        // reresolve() pass can pick it up if the target ever appears.
        let caller = make_node("caller", "function", "lib::caller");
        let nodes = vec![caller.clone()];
        let mut edges = vec![Edge {
            from_id: caller.id.clone(),
            to_id: "__unresolved::missing_target".to_string(),
            kind: "calls".to_string(),
            ref_name: None,
        }];

        crate::graph::extract::resolve_references(&mut edges, &nodes);

        assert_eq!(
            edges.len(),
            1,
            "unresolved edge must be retained, not dropped"
        );
        assert_eq!(edges[0].to_id, "__unresolved::missing_target");
        assert_eq!(
            edges[0].ref_name.as_deref(),
            Some("missing_target"),
            "raw ref token must be captured from the sentinel"
        );
    }

    #[test]
    fn resolve_references_is_rerunnable_same_to_id() {
        // Re-running resolution over edges that already resolved on a prior
        // pass must be a no-op: it resolves from the persisted ref_name, not
        // from the now-overwritten sentinel, so the to_id doesn't regress.
        let caller = make_node("caller", "function", "lib::caller");
        let target = make_node("target", "function", "lib::target");
        let nodes = vec![caller.clone(), target.clone()];
        let mut edges = vec![Edge {
            from_id: caller.id.clone(),
            to_id: "__unresolved::target".to_string(),
            kind: "calls".to_string(),
            ref_name: None,
        }];

        crate::graph::extract::resolve_references(&mut edges, &nodes);
        assert_eq!(edges[0].to_id, target.id, "first pass should resolve");
        assert_eq!(edges[0].ref_name.as_deref(), Some("target"));

        // Second pass over the already-resolved edge + same nodes.
        crate::graph::extract::resolve_references(&mut edges, &nodes);
        assert_eq!(
            edges[0].to_id, target.id,
            "re-running resolution must be idempotent"
        );
    }

    #[test]
    fn reresolve_reconnects_previously_unresolved_edge() {
        // End-to-end: an edge stored unresolved (target absent) stays
        // unresolved across a reresolve() pass; once the target is ingested,
        // a second reresolve() reconnects the SAME stored edge to it.
        let store = GraphStore::open_in_memory().unwrap();
        let caller = make_node("caller", "function", "lib::caller");
        let target = make_node("target", "function", "lib::target");
        let edge = Edge {
            from_id: caller.id.clone(),
            to_id: "__unresolved::target".to_string(),
            kind: "calls".to_string(),
            ref_name: Some("target".to_string()),
        };

        // Target absent: edge is stored unresolved.
        store
            .upsert_source(
                "lib",
                "1.0",
                "rust",
                std::slice::from_ref(&caller),
                std::slice::from_ref(&edge),
            )
            .unwrap();

        let changed = store.reresolve().unwrap();
        assert_eq!(changed, 0, "nothing to connect to yet");
        let stored = store.all_edges_with_refs().unwrap();
        assert_eq!(stored.len(), 1, "unresolved edge must still be stored");
        assert!(
            stored[0].to_id.starts_with("__unresolved::"),
            "got {}",
            stored[0].to_id
        );
        assert_eq!(stored[0].ref_name.as_deref(), Some("target"));

        // Target now present: re-ingest with the same edge, then reresolve.
        store
            .upsert_source(
                "lib",
                "1.0",
                "rust",
                &[caller.clone(), target.clone()],
                std::slice::from_ref(&edge),
            )
            .unwrap();
        let changed = store.reresolve().unwrap();
        assert!(
            changed >= 1,
            "expected at least one edge to reconnect, got {changed}"
        );
        let stored = store.all_edges_with_refs().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].to_id, target.id,
            "edge should now point at the newly-present target"
        );
    }

    #[test]
    fn retained_unresolved_edges_do_not_surface_in_search() {
        // A retained-but-unresolved edge is persisted so it can re-resolve
        // later, but it must never leak into query output as a real edge.
        let store = GraphStore::open_in_memory().unwrap();
        let auth = make_node("authenticate", "function", "lib::authenticate");
        let edge = Edge {
            from_id: auth.id.clone(),
            to_id: "__unresolved::nonexistent".to_string(),
            kind: "calls".to_string(),
            ref_name: Some("nonexistent".to_string()),
        };
        store
            .upsert_source("lib", "1.0", "rust", std::slice::from_ref(&auth), &[edge])
            .unwrap();

        let result = store.search("authenticate", 10).unwrap();
        assert!(
            result.nodes.iter().any(|n| n.id == auth.id),
            "seed node should be present in results"
        );
        assert!(
            !result
                .edges
                .iter()
                .any(|e| e.to_id.starts_with("__unresolved::")),
            "sentinel edges must be filtered from query output, got {:?}",
            result.edges
        );
    }

    // ---- apply_source_delta: byte-identity against a full upsert_source ----

    type FtsRow = (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    );

    /// Canonical dump of a store's nodes, edges, and FTS rows: nodes by `id`,
    /// edges by `(from_id, to_id, kind, ref_name)`, FTS rows by `id`. Two
    /// stores are byte-identical iff all three dumps are equal.
    fn canonical_dump(store: &GraphStore) -> (Vec<Node>, Vec<Edge>, Vec<FtsRow>) {
        let mut nodes = store.all_nodes().unwrap();
        nodes.sort_by(|a, b| a.id.cmp(&b.id));

        let mut edges = store.all_edges_with_refs().unwrap();
        edges.sort_by(|a, b| {
            (&a.from_id, &a.to_id, &a.kind, &a.ref_name).cmp(&(
                &b.from_id,
                &b.to_id,
                &b.kind,
                &b.ref_name,
            ))
        });

        let mut stmt = store
            .conn
            .prepare(
                "SELECT id, name, qualified_name, file_path, signature, doc, body
                 FROM fts_nodes ORDER BY id",
            )
            .unwrap();
        let mut fts: Vec<FtsRow> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        fts.sort();

        (nodes, edges, fts)
    }

    /// Asserts two stores hold byte-identical graphs -- same nodes, same
    /// edges, same FTS rows -- regardless of whether each was built by a full
    /// `upsert_source` rebuild or an incremental `apply_source_delta`.
    fn assert_stores_identical(a: &GraphStore, b: &GraphStore) {
        let (a_nodes, a_edges, a_fts) = canonical_dump(a);
        let (b_nodes, b_edges, b_fts) = canonical_dump(b);
        assert_eq!(a_nodes, b_nodes, "nodes differ between stores");
        assert_eq!(a_edges, b_edges, "edges differ between stores");
        assert_eq!(a_fts, b_fts, "fts_nodes rows differ between stores");
    }

    /// Edges touching `source_name` on EITHER endpoint, sorted by identity --
    /// the same scope `apply_source_delta` reconciles and `remove_source`
    /// deletes, so it isolates whether an update leaked past that scope.
    fn edges_touching_source(store: &GraphStore, source_name: &str) -> Vec<Edge> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT from_id, to_id, kind, ref_name FROM edges
                 WHERE from_id IN (SELECT id FROM nodes WHERE source_name = ?1)
                    OR to_id   IN (SELECT id FROM nodes WHERE source_name = ?1)",
            )
            .unwrap();
        let mut edges: Vec<Edge> = stmt
            .query_map(params![source_name], |row| {
                Ok(Edge {
                    from_id: row.get(0)?,
                    to_id: row.get(1)?,
                    kind: row.get(2)?,
                    ref_name: row.get(3)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        edges.sort_by(|a, b| {
            (&a.from_id, &a.to_id, &a.kind, &a.ref_name).cmp(&(
                &b.from_id,
                &b.to_id,
                &b.kind,
                &b.ref_name,
            ))
        });
        edges
    }

    #[test]
    fn apply_source_delta_updates_unchanged_symbol_after_sibling_line_shift() {
        // fn bravo is defined before fn alpha in the same file, and alpha's
        // body makes no calls. Adding a line inside bravo's body doesn't touch
        // alpha's own source text -- content_hash stays identical -- but
        // alpha's start_line shifts down because bravo grew. "Modified" is
        // full-record inequality, not content_hash alone, so the delta must
        // still update alpha's stored row.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn bravo() {\n    let _x = 1;\n}\n\npub fn alpha() {\n    let _y = 2;\n}\n",
        )
        .unwrap();

        let prior = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();
        let alpha_before = prior
            .nodes
            .iter()
            .find(|n| n.name == "alpha")
            .expect("alpha node")
            .clone();

        let db_dir = tempfile::tempdir().unwrap();
        let delta_store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        delta_store
            .upsert_source("s", "0", "rust", &prior.nodes, &prior.edges)
            .unwrap();

        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn bravo() {\n    let _x = 1;\n    let _z = 2;\n}\n\npub fn alpha() {\n    let _y = 2;\n}\n",
        )
        .unwrap();

        let new = reextract_incremental(dir.path(), "s", "0", Some("rust"), &prior).unwrap();
        let alpha_after = new
            .nodes
            .iter()
            .find(|n| n.name == "alpha")
            .expect("alpha node")
            .clone();
        assert_eq!(
            alpha_after.content_hash, alpha_before.content_hash,
            "alpha's own text is unchanged, so its content_hash must match"
        );
        assert!(
            alpha_after.start_line > alpha_before.start_line,
            "alpha should shift down: before {} after {}",
            alpha_before.start_line,
            alpha_after.start_line
        );

        delta_store
            .apply_source_delta("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        let ref_dir = tempfile::tempdir().unwrap();
        let reference_store = GraphStore::open(&ref_dir.path().join("graph.db")).unwrap();
        reference_store
            .upsert_source("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        assert_stores_identical(&delta_store, &reference_store);

        // A content_hash-only diff would have skipped alpha and left the
        // stale line number in place; confirm the delta actually rewrote it.
        let stored_start_line: i64 = delta_store
            .conn
            .query_row(
                "SELECT start_line FROM nodes WHERE id = ?1",
                params![alpha_after.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_start_line as usize, alpha_after.start_line);
    }

    #[test]
    fn apply_source_delta_replaces_edge_when_target_is_renamed() {
        // A (unchanged) calls target(), defined in B. Renaming target in B
        // must replace the stored concrete A->target edge with an
        // A->__unresolved::target edge -- not leave a stale duplicate
        // alongside it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/b")).unwrap();
        std::fs::write(
            dir.path().join("src/a/mod.rs"),
            "pub fn caller() { target(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/b/mod.rs"), "pub fn target() {}\n").unwrap();

        let prior = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let delta_store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        delta_store
            .upsert_source("s", "0", "rust", &prior.nodes, &prior.edges)
            .unwrap();

        std::fs::write(
            dir.path().join("src/b/mod.rs"),
            "pub fn target_renamed() {}\n",
        )
        .unwrap();

        let new = reextract_incremental(dir.path(), "s", "0", Some("rust"), &prior).unwrap();
        delta_store
            .apply_source_delta("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        let ref_dir = tempfile::tempdir().unwrap();
        let reference_store = GraphStore::open(&ref_dir.path().join("graph.db")).unwrap();
        reference_store
            .upsert_source("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        assert_stores_identical(&delta_store, &reference_store);

        let caller = new.nodes.iter().find(|n| n.name == "caller").unwrap();
        let stored_edges = delta_store.all_edges_with_refs().unwrap();
        let caller_edges: Vec<&Edge> = stored_edges
            .iter()
            .filter(|e| e.from_id == caller.id)
            .collect();
        assert_eq!(
            caller_edges.len(),
            1,
            "expected exactly one edge from caller, no stale duplicate: {caller_edges:?}"
        );
        assert_eq!(caller_edges[0].to_id, "__unresolved::target");
    }

    #[test]
    fn apply_source_delta_resolves_edge_to_newly_added_symbol() {
        // A (unchanged) calls future_fn(), which B does not define yet --
        // stored as a retained unresolved edge. Adding future_fn to B must
        // re-resolve A's kept edge to the new concrete node in the
        // delta-updated store, not leave it dangling on the sentinel.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/caller")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/callee")).unwrap();
        std::fs::write(
            dir.path().join("src/caller/mod.rs"),
            "pub fn invoke() { future_fn(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/callee/mod.rs"),
            "pub fn placeholder() {}\n",
        )
        .unwrap();

        let prior = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();
        assert!(
            prior
                .edges
                .iter()
                .any(|e| e.to_id == "__unresolved::future_fn"),
            "prior should retain an unresolved future_fn call: {:?}",
            prior.edges
        );

        let db_dir = tempfile::tempdir().unwrap();
        let delta_store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        delta_store
            .upsert_source("s", "0", "rust", &prior.nodes, &prior.edges)
            .unwrap();

        std::fs::write(
            dir.path().join("src/callee/mod.rs"),
            "pub fn placeholder() {}\npub fn future_fn() {}\n",
        )
        .unwrap();

        let new = reextract_incremental(dir.path(), "s", "0", Some("rust"), &prior).unwrap();
        delta_store
            .apply_source_delta("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        let ref_dir = tempfile::tempdir().unwrap();
        let reference_store = GraphStore::open(&ref_dir.path().join("graph.db")).unwrap();
        reference_store
            .upsert_source("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        assert_stores_identical(&delta_store, &reference_store);

        let invoke = new.nodes.iter().find(|n| n.name == "invoke").unwrap();
        let future_fn = new.nodes.iter().find(|n| n.name == "future_fn").unwrap();
        let stored_edges = delta_store.all_edges_with_refs().unwrap();
        assert!(
            stored_edges
                .iter()
                .any(|e| e.from_id == invoke.id && e.to_id == future_fn.id && e.kind == "calls"),
            "expected invoke -> future_fn to resolve to the concrete node id: {stored_edges:?}"
        );
    }

    #[test]
    fn apply_source_delta_removes_dangling_edge_to_deleted_symbol() {
        // wants_gadget references gadget(), defined in a separate file.
        // Deleting that file removes gadget's node -- the delta must drop the
        // now-dangling edge along with it, not leave an edge pointing at a
        // removed node id.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/keep_a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/keep_b")).unwrap();
        std::fs::write(
            dir.path().join("src/keep_a/mod.rs"),
            "pub fn wants_gadget() { gadget(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/keep_b/mod.rs"), "pub fn gadget() {}\n").unwrap();

        let prior = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();
        let gadget_id = prior
            .nodes
            .iter()
            .find(|n| n.name == "gadget")
            .expect("gadget node")
            .id
            .clone();
        let wants_gadget = prior
            .nodes
            .iter()
            .find(|n| n.name == "wants_gadget")
            .expect("wants_gadget node")
            .clone();
        assert!(
            prior
                .edges
                .iter()
                .any(|e| e.from_id == wants_gadget.id && e.to_id == gadget_id),
            "prior should have a resolved edge to gadget: {:?}",
            prior.edges
        );

        let db_dir = tempfile::tempdir().unwrap();
        let delta_store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        delta_store
            .upsert_source("s", "0", "rust", &prior.nodes, &prior.edges)
            .unwrap();

        std::fs::remove_file(dir.path().join("src/keep_b/mod.rs")).unwrap();
        std::fs::remove_dir(dir.path().join("src/keep_b")).unwrap();

        let new = reextract_incremental(dir.path(), "s", "0", Some("rust"), &prior).unwrap();
        delta_store
            .apply_source_delta("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        let ref_dir = tempfile::tempdir().unwrap();
        let reference_store = GraphStore::open(&ref_dir.path().join("graph.db")).unwrap();
        reference_store
            .upsert_source("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        assert_stores_identical(&delta_store, &reference_store);

        let dangling: i64 = delta_store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE to_id = ?1",
                params![gadget_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            dangling, 0,
            "no edge should point at the removed gadget node id"
        );
    }

    #[test]
    fn apply_source_delta_reports_exact_stats_for_isolated_single_node_change() {
        // An edit local enough to change exactly one node's stored content,
        // touching no edges: DeltaStats must report that precisely, not a
        // coarser count from some broader rescan.
        let db_dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        let isolated = make_node("isolated", "function", "test::isolated");
        let other = make_node("other", "function", "test::other");
        store
            .upsert_source("test", "0", "rust", &[isolated.clone(), other.clone()], &[])
            .unwrap();

        let mut changed = isolated.clone();
        changed.body = format!("{} // literal changed", isolated.body);
        changed.content_hash = Some("changed-hash".to_string());

        let stats = store
            .apply_source_delta("test", "0", "rust", &[changed, other], &[])
            .unwrap();

        assert_eq!(
            stats,
            DeltaStats {
                nodes_added: 0,
                nodes_modified: 1,
                nodes_removed: 0,
                edges_added: 0,
                edges_removed: 0,
            }
        );
    }

    #[test]
    fn apply_source_delta_leaves_other_sources_nodes_and_edges_untouched() {
        // Applying a delta to s1 must never touch s2's rows -- guards the
        // edge-scoping (and node scoping) against a full-table wipe.
        let dir1 = tempfile::tempdir().unwrap();
        std::fs::write(dir1.path().join("lib.rs"), "pub fn one() {}\n").unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(
            dir2.path().join("lib.rs"),
            "pub fn two() { two_helper(); }\npub fn two_helper() {}\n",
        )
        .unwrap();

        let prior1 = extract_dir(dir1.path(), "s1", "0", Some("rust")).unwrap();
        let g2 = extract_dir(dir2.path(), "s2", "0", Some("rust")).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        store
            .upsert_source("s1", "0", "rust", &prior1.nodes, &prior1.edges)
            .unwrap();
        store
            .upsert_source("s2", "0", "rust", &g2.nodes, &g2.edges)
            .unwrap();

        let mut s2_nodes_before: Vec<Node> = store
            .all_nodes()
            .unwrap()
            .into_iter()
            .filter(|n| n.source_name == "s2")
            .collect();
        s2_nodes_before.sort_by(|a, b| a.id.cmp(&b.id));
        let s2_edges_before = edges_touching_source(&store, "s2");

        std::fs::write(
            dir1.path().join("lib.rs"),
            "pub fn one() { let _changed = 1; }\n",
        )
        .unwrap();
        let new1 = reextract_incremental(dir1.path(), "s1", "0", Some("rust"), &prior1).unwrap();
        store
            .apply_source_delta("s1", "0", "rust", &new1.nodes, &new1.edges)
            .unwrap();

        let mut s2_nodes_after: Vec<Node> = store
            .all_nodes()
            .unwrap()
            .into_iter()
            .filter(|n| n.source_name == "s2")
            .collect();
        s2_nodes_after.sort_by(|a, b| a.id.cmp(&b.id));
        let s2_edges_after = edges_touching_source(&store, "s2");

        assert_eq!(
            s2_nodes_before, s2_nodes_after,
            "s2 nodes must be untouched by s1's delta"
        );
        assert_eq!(
            s2_edges_before, s2_edges_after,
            "s2 edges must be untouched by s1's delta"
        );
    }

    #[test]
    fn apply_source_delta_drops_cross_source_edge_pointing_into_this_source() {
        // A full rebuild of s1 (remove_source, matching upsert_source's wipe)
        // drops every edge touching s1 on EITHER endpoint, including one from
        // another source into s1. The delta must scope its edge cleanup the
        // same way -- not just by from_id -- or a foreign edge into a
        // delta-updated source survives where a full rebuild would drop it.
        let dir1 = tempfile::tempdir().unwrap();
        std::fs::write(dir1.path().join("lib.rs"), "pub fn one() {}\n").unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        std::fs::write(dir2.path().join("lib.rs"), "pub fn two() {}\n").unwrap();

        let g1 = extract_dir(dir1.path(), "s1", "0", Some("rust")).unwrap();
        let g2 = extract_dir(dir2.path(), "s2", "0", Some("rust")).unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        store
            .upsert_source("s1", "0", "rust", &g1.nodes, &g1.edges)
            .unwrap();
        store
            .upsert_source("s2", "0", "rust", &g2.nodes, &g2.edges)
            .unwrap();

        let s2_node = g2.nodes.iter().find(|n| n.name == "two").unwrap();
        let s1_node = g1.nodes.iter().find(|n| n.name == "one").unwrap();
        store
            .conn
            .execute(
                "INSERT OR REPLACE INTO edges (from_id, to_id, kind, ref_name) VALUES (?1, ?2, ?3, ?4)",
                params![s2_node.id, s1_node.id, "calls", Some("x")],
            )
            .unwrap();

        let cross_before: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE from_id = ?1 AND to_id = ?2",
                params![s2_node.id, s1_node.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            cross_before, 1,
            "test setup: cross-source edge should exist"
        );

        // s1's own graph is unchanged -- the delta still must reconcile s1's
        // edge scope against everything stored, including foreign edges.
        store
            .apply_source_delta("s1", "0", "rust", &g1.nodes, &g1.edges)
            .unwrap();

        let cross_after: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE from_id = ?1 AND to_id = ?2",
                params![s2_node.id, s1_node.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            cross_after, 0,
            "cross-source edge into s1 must be dropped by the delta, matching a full rebuild"
        );
    }

    #[test]
    fn update_via_db_prior_matches_full_rebuild() {
        // The property that makes `roux update` safe: reconstructing the prior
        // graph FROM THE DB (`source_graph`, which loads rows in
        // (file_path, start_line, id) order -- NOT the walk/emission order
        // `extract_dir` produces) and feeding it to `reextract_incremental`
        // must still yield a store byte-identical to a full rebuild. This only
        // holds because `finalize_graph` canonicalizes node/edge order itself,
        // making it a pure function of the node/edge SETS.
        //
        // The edit exercises cross-file re-resolution on every axis: renaming
        // a called fn in B breaks A's (unchanged) call; adding a new fn in B
        // resolves D's (unchanged) previously-unresolved call; a file is added;
        // a file is deleted.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/b")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/d")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/f")).unwrap();
        std::fs::write(
            dir.path().join("src/a/mod.rs"),
            "pub fn caller() { helper(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/b/mod.rs"), "pub fn helper() {}\n").unwrap();
        std::fs::write(
            dir.path().join("src/d/mod.rs"),
            "pub fn waiter() { future_fn(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/f/mod.rs"), "pub fn to_delete() {}\n").unwrap();

        let full1 = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();
        assert!(
            full1
                .edges
                .iter()
                .any(|e| e.to_id == "__unresolved::future_fn"),
            "test setup: waiter's call to future_fn should start out unresolved: {:?}",
            full1.edges
        );

        let db_dir_a = tempfile::tempdir().unwrap();
        let store_a = GraphStore::open(&db_dir_a.path().join("graph.db")).unwrap();
        store_a
            .upsert_source("s", "0", "rust", &full1.nodes, &full1.edges)
            .unwrap();
        store_a.replace_files("s", &full1.files).unwrap();

        // Rename the called fn in B (breaks A's unchanged call) and add a new
        // fn in B that D (unchanged) already referenced.
        std::fs::write(
            dir.path().join("src/b/mod.rs"),
            "pub fn helper_v2() {}\npub fn future_fn() {}\n",
        )
        .unwrap();
        // Delete a file.
        std::fs::remove_file(dir.path().join("src/f/mod.rs")).unwrap();
        std::fs::remove_dir(dir.path().join("src/f")).unwrap();
        // Add a new file.
        std::fs::create_dir_all(dir.path().join("src/g")).unwrap();
        std::fs::write(dir.path().join("src/g/mod.rs"), "pub fn new_file_fn() {}\n").unwrap();

        // Reconstruct the prior FROM THE DB -- not from the in-memory `full1`
        // -- so the test actually exercises `source_graph`'s DB-order load.
        let prior = store_a.source_graph("s").unwrap();
        let new = reextract_incremental(dir.path(), "s", "0", Some("rust"), &prior).unwrap();
        store_a
            .apply_source_delta("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();
        store_a.replace_files("s", &new.files).unwrap();

        // Reference: an independent full rebuild of the current tree into a
        // fresh store.
        let full2 = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();
        let db_dir_b = tempfile::tempdir().unwrap();
        let store_b = GraphStore::open(&db_dir_b.path().join("graph.db")).unwrap();
        store_b
            .upsert_source("s", "0", "rust", &full2.nodes, &full2.edges)
            .unwrap();
        store_b.replace_files("s", &full2.files).unwrap();

        // The key assertion: a DB-order prior still yields a rebuild-identical
        // result.
        assert_stores_identical(&store_a, &store_b);

        // Sanity: the edit actually landed the cross-file re-resolution it was
        // designed to exercise, so the byte-identity assertion above isn't
        // vacuously comparing two empty/unrelated graphs.
        let caller = new.nodes.iter().find(|n| n.name == "caller").unwrap();
        let waiter = new.nodes.iter().find(|n| n.name == "waiter").unwrap();
        let future_fn = new.nodes.iter().find(|n| n.name == "future_fn").unwrap();
        assert!(
            new.edges
                .iter()
                .any(|e| e.from_id == caller.id && e.to_id == "__unresolved::helper"),
            "renaming helper in B should break A's unchanged call: {:?}",
            new.edges
        );
        assert!(
            new.edges
                .iter()
                .any(|e| e.from_id == waiter.id && e.to_id == future_fn.id && e.kind == "calls"),
            "D's unchanged call to future_fn should resolve once B adds it: {:?}",
            new.edges
        );
        assert!(
            new.nodes.iter().any(|n| n.name == "new_file_fn"),
            "the newly added file's fn should be present"
        );
        assert!(
            !new.nodes.iter().any(|n| n.name == "to_delete"),
            "the deleted file's fn should not survive re-extraction"
        );
    }

    #[test]
    fn apply_source_delta_via_db_prior_is_idempotent_on_unchanged_tree() {
        // A refresh with no working-tree changes, driven end-to-end through a
        // DB-reconstructed prior, must write nothing: DeltaStats all zero.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::create_dir_all(dir.path().join("src/b")).unwrap();
        std::fs::write(
            dir.path().join("src/a/mod.rs"),
            "pub fn caller() { helper(); }\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("src/b/mod.rs"), "pub fn helper() {}\n").unwrap();

        let full = extract_dir(dir.path(), "s", "0", Some("rust")).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let store = GraphStore::open(&db_dir.path().join("graph.db")).unwrap();
        store
            .upsert_source("s", "0", "rust", &full.nodes, &full.edges)
            .unwrap();
        store.replace_files("s", &full.files).unwrap();

        // No filesystem edits at all: reconstruct the prior from the DB and
        // re-extract against the untouched tree.
        let prior = store.source_graph("s").unwrap();
        let new = reextract_incremental(dir.path(), "s", "0", Some("rust"), &prior).unwrap();
        let stats = store
            .apply_source_delta("s", "0", "rust", &new.nodes, &new.edges)
            .unwrap();

        assert_eq!(
            stats,
            DeltaStats::default(),
            "a no-change refresh should write nothing: {stats:?}"
        );
    }
}
