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

        if version < 2 {
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
                    parent_id      TEXT
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
                "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', '2')",
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

        // Remove old data for this source
        {
            let ids: Vec<String> = {
                let mut stmt = tx.prepare("SELECT id FROM nodes WHERE source_name = ?1")?;
                stmt.query_map(params![source_name], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };

            for id in &ids {
                tx.execute(
                    "DELETE FROM edges WHERE from_id = ?1 OR to_id = ?1",
                    params![id],
                )?;
                tx.execute("DELETE FROM fts_nodes WHERE id = ?1", params![id])?;
            }
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
                     signature, doc, body, parent_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
                ])?;
                fts_stmt.execute(params![
                    node.id,
                    node.name,
                    node.qualified_name,
                    node.file_path,
                    node.signature,
                    node.doc,
                    node.body,
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
        let safe_query = fts_query_escape(query);
        if safe_query.is_empty() {
            return Ok(SearchResult::default());
        }

        // BM25 search on FTS index
        let matched_ids: Vec<String> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM fts_nodes WHERE fts_nodes MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            stmt.query_map(params![safe_query, limit as i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        if matched_ids.is_empty() {
            return Ok(SearchResult::default());
        }

        // Expand: matched nodes + 1-hop via edges + children via parent_id + parents
        let mut all_ids: Vec<String> = matched_ids.clone();

        for id in &matched_ids {
            // Edge neighbors (both directions)
            let mut stmt = self.conn.prepare_cached(
                "SELECT to_id FROM edges WHERE from_id = ?1
                 UNION
                 SELECT from_id FROM edges WHERE to_id = ?1",
            )?;
            let neighbors: Vec<String> = stmt
                .query_map(params![id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all_ids.extend(neighbors);

            // Parent
            let parent: Option<String> = self
                .conn
                .query_row(
                    "SELECT parent_id FROM nodes WHERE id = ?1 AND parent_id IS NOT NULL",
                    params![id],
                    |row| row.get(0),
                )
                .ok();
            if let Some(pid) = parent {
                all_ids.push(pid);
            }

            // Children
            let mut stmt = self
                .conn
                .prepare_cached("SELECT id FROM nodes WHERE parent_id = ?1")?;
            let children: Vec<String> = stmt
                .query_map(params![id], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            all_ids.extend(children);
        }

        all_ids.sort();
        all_ids.dedup();

        let nodes = self.fetch_nodes(&all_ids)?;
        let edges = self.fetch_edges(&all_ids)?;

        Ok(SearchResult {
            matched_ids,
            nodes,
            edges,
        })
    }

    fn fetch_nodes(&self, ids: &[String]) -> Result<Vec<Node>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }

        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "SELECT id, kind, name, qualified_name, source_name, language,
                    file_path, start_line, start_col, end_line, visibility,
                    signature, doc, body, parent_id
             FROM nodes WHERE id IN ({})",
            placeholders.join(", ")
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = ids
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(nodes)
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
            let mut stmt = tx.prepare("SELECT id FROM nodes WHERE source_name = ?1")?;
            stmt.query_map(params![name], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        for id in &ids {
            tx.execute(
                "DELETE FROM edges WHERE from_id = ?1 OR to_id = ?1",
                params![id],
            )?;
            tx.execute("DELETE FROM fts_nodes WHERE id = ?1", params![id])?;
        }
        tx.execute("DELETE FROM nodes WHERE source_name = ?1", params![name])?;
        tx.execute("DELETE FROM sources WHERE name = ?1", params![name])?;

        tx.commit()?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.version, s.language, s.ingested_at,
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
                    node_count: row.get(4)?,
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

#[derive(Debug, Default)]
pub struct SearchResult {
    pub matched_ids: Vec<String>,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub struct SourceRecord {
    pub name: String,
    pub version: String,
    pub language: String,
    pub ingested_at: i64,
    pub node_count: usize,
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
        assert_eq!(version, "2");
    }
}
