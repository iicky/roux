use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::{Edge, Symbol};

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
        store.create_tables()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.create_tables()?;
        Ok(store)
    }

    fn create_tables(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS symbols (
                id             TEXT PRIMARY KEY,
                kind           TEXT NOT NULL,
                name           TEXT NOT NULL,
                qualified_name TEXT NOT NULL,
                source_name    TEXT NOT NULL,
                source_version TEXT NOT NULL,
                language       TEXT NOT NULL,
                file_path      TEXT NOT NULL,
                line           INTEGER NOT NULL,
                signature      TEXT,
                doc            TEXT,
                body           TEXT NOT NULL,
                parent_id      TEXT REFERENCES symbols(id)
            );

            CREATE INDEX IF NOT EXISTS idx_symbols_source ON symbols(source_name);
            CREATE INDEX IF NOT EXISTS idx_symbols_parent ON symbols(parent_id);
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);

            CREATE TABLE IF NOT EXISTS edges (
                from_id TEXT NOT NULL REFERENCES symbols(id),
                to_id   TEXT NOT NULL REFERENCES symbols(id),
                kind    TEXT NOT NULL,
                PRIMARY KEY (from_id, to_id, kind)
            );

            CREATE INDEX IF NOT EXISTS idx_edges_from ON edges(from_id);
            CREATE INDEX IF NOT EXISTS idx_edges_to ON edges(to_id);

            CREATE TABLE IF NOT EXISTS sources (
                name        TEXT PRIMARY KEY,
                version     TEXT NOT NULL,
                language    TEXT NOT NULL,
                ingested_at INTEGER NOT NULL
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS fts_symbols USING fts5(
                id UNINDEXED,
                name,
                qualified_name,
                signature,
                doc,
                body,
                tokenize='unicode61 remove_diacritics 2'
            );

            CREATE TABLE IF NOT EXISTS metadata (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    /// Insert symbols and edges for a source, replacing any existing data.
    pub fn upsert_source(
        &self,
        source_name: &str,
        source_version: &str,
        language: &str,
        symbols: &[Symbol],
        edges: &[Edge],
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        // Remove old data for this source
        {
            let ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM symbols WHERE source_name = ?1")?;
                stmt.query_map(params![source_name], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };

            for id in &ids {
                tx.execute(
                    "DELETE FROM edges WHERE from_id = ?1 OR to_id = ?1",
                    params![id],
                )?;
                tx.execute("DELETE FROM fts_symbols WHERE id = ?1", params![id])?;
            }
            tx.execute(
                "DELETE FROM symbols WHERE source_name = ?1",
                params![source_name],
            )?;
        }

        // Insert symbols
        {
            let mut sym_stmt = tx.prepare_cached(
                "INSERT OR REPLACE INTO symbols
                    (id, kind, name, qualified_name, source_name, source_version,
                     language, file_path, line, signature, doc, body, parent_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            )?;
            let mut fts_stmt = tx.prepare_cached(
                "INSERT INTO fts_symbols (id, name, qualified_name, signature, doc, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;

            for sym in symbols {
                sym_stmt.execute(params![
                    sym.id,
                    sym.kind,
                    sym.name,
                    sym.qualified_name,
                    sym.source_name,
                    sym.source_version,
                    sym.language,
                    sym.file_path,
                    sym.line,
                    sym.signature,
                    sym.doc,
                    sym.body,
                    sym.parent_id,
                ])?;
                fts_stmt.execute(params![
                    sym.id,
                    sym.name,
                    sym.qualified_name,
                    sym.signature,
                    sym.doc,
                    sym.body,
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

        // Update source record
        tx.execute(
            "INSERT OR REPLACE INTO sources (name, version, language, ingested_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                source_name,
                source_version,
                language,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Search by keyword, return matching symbols + their graph neighborhood.
    pub fn search(&self, query: &str, limit: usize) -> Result<SearchResult> {
        let safe_query = fts_query_escape(query);
        if safe_query.is_empty() {
            return Ok(SearchResult::default());
        }

        // BM25 search on FTS index
        let matched_ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM fts_symbols WHERE fts_symbols MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            stmt.query_map(params![safe_query, limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        if matched_ids.is_empty() {
            return Ok(SearchResult::default());
        }

        // Expand: get the matched symbols + 1-hop neighborhood
        let mut all_ids: Vec<String> = matched_ids.clone();

        // Get neighbors via edges (both directions)
        for id in &matched_ids {
            let mut stmt = self.conn.prepare_cached(
                "SELECT to_id FROM edges WHERE from_id = ?1
                 UNION
                 SELECT from_id FROM edges WHERE to_id = ?1",
            )?;
            let neighbors: Vec<String> = stmt
                .query_map(params![id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all_ids.extend(neighbors);
        }

        // Also include parents of matched symbols
        for id in &matched_ids {
            let parent: Option<String> = self
                .conn
                .query_row(
                    "SELECT parent_id FROM symbols WHERE id = ?1 AND parent_id IS NOT NULL",
                    params![id],
                    |row| row.get(0),
                )
                .ok();
            if let Some(pid) = parent {
                all_ids.push(pid);
            }
        }

        // Deduplicate
        all_ids.sort();
        all_ids.dedup();

        // Fetch all symbols
        let symbols = self.fetch_symbols(&all_ids)?;

        // Fetch edges between these symbols
        let edges = self.fetch_edges(&all_ids)?;

        Ok(SearchResult {
            matched_ids,
            symbols,
            edges,
        })
    }

    fn fetch_symbols(&self, ids: &[String]) -> Result<Vec<Symbol>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT id, kind, name, qualified_name, source_name, source_version,
                    language, file_path, line, signature, doc, body, parent_id
             FROM symbols WHERE id IN ({})",
            placeholders.join(", ")
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();

        let symbols = stmt
            .query_map(params.as_slice(), |row| {
                Ok(Symbol {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    name: row.get(2)?,
                    qualified_name: row.get(3)?,
                    source_name: row.get(4)?,
                    source_version: row.get(5)?,
                    language: row.get(6)?,
                    file_path: row.get(7)?,
                    line: row.get(8)?,
                    signature: row.get(9)?,
                    doc: row.get(10)?,
                    body: row.get(11)?,
                    parent_id: row.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(symbols)
    }

    fn fetch_edges(&self, ids: &[String]) -> Result<Vec<Edge>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let n = ids.len();
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
        for id in ids {
            all_params.push(id as &dyn rusqlite::types::ToSql);
        }
        for id in ids {
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

        Ok(edges)
    }

    pub fn remove_source(&self, name: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;

        let ids: Vec<String> = {
            let mut stmt = tx.prepare("SELECT id FROM symbols WHERE source_name = ?1")?;
            stmt.query_map(params![name], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        for id in &ids {
            tx.execute(
                "DELETE FROM edges WHERE from_id = ?1 OR to_id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM fts_symbols WHERE id = ?1", params![id])?;
        }
        tx.execute("DELETE FROM symbols WHERE source_name = ?1", params![name])?;
        tx.execute("DELETE FROM sources WHERE name = ?1", params![name])?;

        tx.commit()?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.version, s.language, s.ingested_at,
                    COUNT(sym.id) as symbol_count
             FROM sources s
             LEFT JOIN symbols sym ON sym.source_name = s.name
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
                    symbol_count: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(records)
    }
}

#[derive(Debug, Default)]
pub struct SearchResult {
    /// IDs of symbols that directly matched the query
    pub matched_ids: Vec<String>,
    /// All symbols in the result subgraph (matches + neighborhood)
    pub symbols: Vec<Symbol>,
    /// All edges between symbols in the subgraph
    pub edges: Vec<Edge>,
}

pub struct SourceRecord {
    pub name: String,
    pub version: String,
    pub language: String,
    pub ingested_at: i64,
    pub symbol_count: usize,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_symbol(name: &str, kind: &str, qualified: &str) -> Symbol {
        let id = Symbol::id_for("test", qualified);
        Symbol {
            id: id.clone(),
            kind: kind.to_string(),
            name: name.to_string(),
            qualified_name: qualified.to_string(),
            source_name: "test".to_string(),
            source_version: "1.0.0".to_string(),
            language: "rust".to_string(),
            file_path: "src/lib.rs".to_string(),
            line: 1,
            signature: Some(format!("fn {name}()")),
            doc: Some(format!("Does {name} things.")),
            body: format!("function: {qualified}\nfn {name}()\nDoes {name} things."),
            parent_id: None,
        }
    }

    #[test]
    fn test_upsert_and_search() {
        let store = GraphStore::open_in_memory().unwrap();

        let sym = make_symbol("spawn", "function", "tokio::spawn");
        store
            .upsert_source("test", "1.0.0", "rust", &[sym], &[])
            .unwrap();

        let result = store.search("spawn", 5).unwrap();
        assert_eq!(result.matched_ids.len(), 1);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].name, "spawn");
    }

    #[test]
    fn test_graph_expansion() {
        let store = GraphStore::open_in_memory().unwrap();

        let auth = make_symbol("authenticate", "function", "auth::authenticate");
        let validate = make_symbol("validate_token", "function", "auth::validate_token");
        let hash = make_symbol("hash_password", "function", "auth::hash_password");

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

        // Search for authenticate — should also return validate_token and hash_password
        let result = store.search("authenticate", 5).unwrap();
        assert_eq!(result.matched_ids.len(), 1);
        assert_eq!(
            result.symbols.len(),
            3,
            "should expand to include called symbols"
        );
        assert_eq!(result.edges.len(), 2);
    }

    #[test]
    fn test_parent_expansion() {
        let store = GraphStore::open_in_memory().unwrap();

        let module = make_symbol("auth", "module", "myapp::auth");
        let mut func = make_symbol("login", "function", "myapp::auth::login");
        func.parent_id = Some(module.id.clone());

        store
            .upsert_source("test", "1.0.0", "rust", &[module, func], &[])
            .unwrap();

        // Search for login — should also return the parent module
        let result = store.search("login", 5).unwrap();
        assert_eq!(result.matched_ids.len(), 1);
        assert_eq!(
            result.symbols.len(),
            2,
            "should expand to include parent module"
        );
    }

    #[test]
    fn test_remove_source() {
        let store = GraphStore::open_in_memory().unwrap();

        let sym = make_symbol("foo", "function", "lib::foo");
        store
            .upsert_source("test", "1.0.0", "rust", &[sym], &[])
            .unwrap();

        store.remove_source("test").unwrap();

        let result = store.search("foo", 5).unwrap();
        assert!(result.symbols.is_empty());
    }

    #[test]
    fn test_list_sources() {
        let store = GraphStore::open_in_memory().unwrap();

        let syms = vec![
            make_symbol("foo", "function", "mylib::foo"),
            make_symbol("bar", "function", "mylib::bar"),
        ];
        // make_symbol uses "test" as source_name — override for this test
        let syms: Vec<_> = syms
            .into_iter()
            .map(|mut s| {
                s.source_name = "mylib".to_string();
                s
            })
            .collect();
        store
            .upsert_source("mylib", "2.0.0", "rust", &syms, &[])
            .unwrap();

        let sources = store.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].name, "mylib");
        assert_eq!(sources[0].symbol_count, 2);
    }

    #[test]
    fn test_empty_search() {
        let store = GraphStore::open_in_memory().unwrap();
        let result = store.search("nonexistent", 5).unwrap();
        assert!(result.symbols.is_empty());
    }
}
