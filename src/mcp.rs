//! MCP (Model Context Protocol) stdio server. Wraps existing CLI query/list
//! logic so AI agents (Claude Code, Cursor, etc.) can call roux directly
//! without shell prompts, eliminating per-call permission overhead.

use std::path::PathBuf;

use anyhow::Result;
use percent_encoding::percent_decode_str;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        Annotated, CallToolResult, Content, Implementation, ListResourceTemplatesResult,
        PaginatedRequestParams, RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult,
        ResourceContents, ServerCapabilities, ServerInfo,
    },
    schemars,
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;

use crate::cli::{
    check_source_status, list_rows_to_json, render_compact, render_skeleton,
    search_result_to_json,
};
use crate::graph::store::GraphStore;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryArgs {
    /// Search query — natural language or keywords. Matches symbol names,
    /// signatures, docstrings, and bodies. Returned hits include 2-hop graph
    /// neighbors (callers, callees, parent types).
    pub query: String,
    /// Optional extra query variants, fused with `query` via reciprocal-rank
    /// fusion in one call. Use your domain knowledge to reformulate a plain
    /// question into the jargon/identifiers the code likely uses, then pass
    /// them here — e.g. for "limit sudden jolts" pass
    /// ["jerk limit", "junction deviation", "M205"]. This is the main lever
    /// for conceptual/behavioral questions where the code's words differ from
    /// the user's; firing several cheap variants beats one broad query.
    #[serde(default)]
    pub queries: Option<Vec<String>>,
    /// Number of matched hits to return. Defaults to 5.
    #[serde(default)]
    pub top: Option<usize>,
    /// Restrict search to a single indexed source (use roux_list to see names).
    #[serde(default)]
    pub source: Option<String>,
    /// Return a compact text block (ranked matches with signature, one-line doc,
    /// and neighbor names under a token budget) instead of the full JSON graph.
    /// Much smaller — prefer it unless you need ids/scores/edges. Defaults false.
    #[serde(default)]
    pub compact: Option<bool>,
}

#[derive(Clone)]
pub struct RouxServer {
    store_path: PathBuf,
    tool_router: ToolRouter<RouxServer>,
}

#[tool_router]
impl RouxServer {
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            tool_router: Self::tool_router(),
        }
    }

    fn open_store(&self) -> Result<GraphStore, McpError> {
        GraphStore::open(&self.store_path)
            .map_err(|e| McpError::internal_error(format!("open index: {e}"), None))
    }

    #[tool(
        description = "Search the roux code index. Returns matched symbols plus their graph neighborhood (callers, callees, parent types) — typically more useful than a flat list. Prefer this over grep for code-exploration questions; results carry file path, line, signature, and rendered doc.\n\nRanking is BM25 over symbol names, signatures, and qualified paths, so queries that share tokens with the symbol name work best. For conceptual or behavioral questions (\"how does X work\", \"where is Y handled\") the code rarely uses the user's words — so reformulate into the jargon and identifiers the code likely uses and pass them together in the `queries` array (RRF-fused in one call). E.g. for \"limit sudden jolts when speed changes\" pass queries=[\"jerk limit\", \"junction deviation\", \"M205\"]. Measured to recover behavioral hits a single literal query misses. Each call is cheap; reformulating beats one broad query.\n\nPass compact=true to get a small text block (ranked matches with signature, one-line doc, and neighbor names under a token budget) instead of the full JSON graph — prefer it to keep context small unless you specifically need ids, scores, or edges."
    )]
    fn roux_query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.open_store()?;
        // Bound `top` so a caller can't request an unreasonable result set.
        const MAX_TOP: usize = 1000;
        let top = args.top.unwrap_or(5).clamp(1, MAX_TOP);

        // Validate the source against the index up front so an unknown name
        // gives an actionable error listing what's available, rather than an
        // opaque failure deeper in the search.
        if let Some(src) = args.source.as_deref() {
            let known = store
                .list_sources()
                .map_err(|e| McpError::internal_error(format!("list sources: {e}"), None))?;
            if !known.iter().any(|s| s.name == src) {
                let names: Vec<&str> = known.iter().map(|s| s.name.as_str()).collect();
                return Err(McpError::invalid_params(
                    format!("unknown source {src:?}; available: [{}]", names.join(", ")),
                    None,
                ));
            }
        }

        let mut queries = vec![args.query];
        if let Some(extra) = args.queries {
            queries.extend(extra);
        }
        let result = store
            .search_multi(&queries, top, args.source.as_deref())
            .map_err(|e| McpError::internal_error(format!("search: {e}"), None))?;
        if args.compact.unwrap_or(false) {
            return Ok(CallToolResult::success(vec![Content::text(
                render_compact(&result),
            )]));
        }
        // Staleness guard (roux-00bf): surface changed-since-indexing files so a
        // live agent doesn't trust stale locations. Same `stale` block as the CLI.
        let mut json = search_result_to_json(&result);
        let stale = crate::cli::stale_sources_for_result(&store, &result);
        if !stale.is_empty()
            && let Some(obj) = json.as_object_mut()
        {
            obj.insert("stale".into(), crate::cli::stale_to_json(&stale));
        }
        let body = serde_json::to_string_pretty(&json)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "List indexed sources at this index location, with version, language, symbol count, and freshness status."
    )]
    fn roux_list(&self) -> Result<CallToolResult, McpError> {
        let store = self.open_store()?;
        let sources = store
            .list_sources()
            .map_err(|e| McpError::internal_error(format!("list sources: {e}"), None))?;
        let rows: Vec<_> = sources
            .into_iter()
            .map(|src| {
                let status = check_source_status(&src);
                let display = src.name.clone();
                (src, status, display)
            })
            .collect();
        let json = list_rows_to_json(&rows);
        let body = serde_json::to_string_pretty(&json)
            .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Quick health check: index path, source count, and total indexed symbols. Use this before querying to confirm the index is populated."
    )]
    fn roux_status(&self) -> Result<CallToolResult, McpError> {
        let store = self.open_store()?;
        let sources = store
            .list_sources()
            .map_err(|e| McpError::internal_error(format!("list sources: {e}"), None))?;
        let total_symbols: usize = sources.iter().map(|s| s.node_count).sum();
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "store_path": self.store_path.display().to_string(),
            "source_count": sources.len(),
            "total_symbols": total_symbols,
        }))
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

/// URI prefix for the query-scoped skeleton resource template. A read of
/// `roux://skeleton/<url-encoded query>` returns the compact ranked skeleton
/// block — the same bytes as `roux query --format skeleton`.
const SKELETON_URI_PREFIX: &str = "roux://skeleton/";

/// Hits rendered into a skeleton read. Larger than the interactive tool default
/// (this is a one-shot prefix injected once and prompt-cached, so a fuller map
/// costs almost nothing on subsequent turns) but fixed, so the block is stable
/// per query and caches cleanly.
const SKELETON_TOP: usize = 10;

#[tool_handler]
impl ServerHandler for RouxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_instructions(
            "Graph-native code retrieval for AI agents. Use roux_query for symbol search; \
             results include 2-hop graph neighborhood (callers, callees, parent types). \
             roux_list shows what's indexed; roux_status reports index health.\n\n\
             Ranking is name-biased BM25 — for behavioral questions (\"how does X work\"), \
             follow up with 2–3 likely symbol-name variants in separate calls rather than \
             one verbose query.\n\n\
             To spend fewer tokens, read the `roux://skeleton/{query}` resource ONCE at the \
             start of a task instead of calling roux_query every turn: it returns a compact \
             ranked skeleton you can keep in context and prompt-cache, avoiding the per-turn \
             tool-schema and extra-turn cost of live calls."
                .to_string(),
        )
    }

    /// Advertise the one-shot skeleton as a resource template. Clients read it
    /// once and inject the block into the prompt prefix (prompt-cacheable),
    /// rather than paying the per-turn cost of a live `roux_query` tool call.
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let tmpl = RawResourceTemplate {
            uri_template: format!("{SKELETON_URI_PREFIX}{{query}}"),
            name: "roux skeleton".to_string(),
            title: Some("roux ranked code skeleton".to_string()),
            description: Some(
                "One-shot code-context preprocessor: reads a compact ranked skeleton \
                 (qualified name, file:line, signature, one-line doc) of the symbols most \
                 relevant to {query}. Read once up front and keep in context instead of \
                 calling roux_query per turn — the block is prompt-cacheable and holds \
                 agent input tokens below a no-tool baseline. {query} is a natural-language \
                 or keyword search string, URL-encoded."
                    .to_string(),
            ),
            mime_type: Some("text/plain".to_string()),
            icons: None,
        };
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            Annotated::new(tmpl, None),
        ]))
    }

    /// Render the skeleton for `roux://skeleton/<url-encoded query>`. Reuses the
    /// exact search + `render_skeleton` path behind `roux query --format
    /// skeleton`, so the resource bytes match the measured context-prep arm.
    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let block = self.render_skeleton_uri(&request.uri)?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            block,
            request.uri,
        )]))
    }
}

impl RouxServer {
    /// Core of `read_resource`, split out so it is testable without fabricating
    /// a `RequestContext`: parse `roux://skeleton/<url-encoded query>` and render
    /// the skeleton block for that query.
    fn render_skeleton_uri(&self, uri: &str) -> Result<String, McpError> {
        let raw = uri.strip_prefix(SKELETON_URI_PREFIX).ok_or_else(|| {
            McpError::resource_not_found(
                format!("unknown resource {uri:?}; expected {SKELETON_URI_PREFIX}{{query}}"),
                None,
            )
        })?;
        let query = percent_decode_str(raw).decode_utf8_lossy();
        let query = query.trim();
        if query.is_empty() {
            return Err(McpError::invalid_params(
                format!("empty query in resource URI {uri:?}"),
                None,
            ));
        }

        let store = self.open_store()?;
        let result = store
            .search_scoped(query, SKELETON_TOP, None)
            .map_err(|e| McpError::internal_error(format!("search: {e}"), None))?;
        Ok(render_skeleton(&result))
    }
}

/// Run the MCP server over stdio until the client disconnects. Spins up a
/// single-threaded tokio runtime; nothing else in the binary is async.
pub fn run_stdio(store_path: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let service = RouxServer::new(store_path).serve(stdio()).await?;
        service.waiting().await?;
        anyhow::Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Node;

    fn server_with_source(name: &str) -> (RouxServer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        {
            let store = GraphStore::open(&path).unwrap();
            let node = Node {
                id: Node::id_for(name, &format!("{name}::foo")),
                kind: "function".into(),
                name: "foo".into(),
                qualified_name: format!("{name}::foo"),
                source_name: name.into(),
                language: "rust".into(),
                file_path: "lib.rs".into(),
                start_line: 1,
                start_col: 0,
                end_line: 2,
                visibility: "pub".into(),
                signature: Some("fn foo()".into()),
                doc: None,
                body: "fn foo()".into(),
                parent_id: None,
                content_hash: None,
                line_count: 1,
                source_url: None,
                description: None,
            };
            store.upsert_source(name, "1.0", "rust", &[node], &[]).unwrap();
        }
        (RouxServer::new(path), dir)
    }

    fn query(source: Option<&str>) -> QueryArgs {
        QueryArgs {
            query: "foo".into(),
            queries: None,
            top: None,
            source: source.map(str::to_string),
            compact: None,
        }
    }

    #[test]
    fn unknown_source_is_rejected() {
        let (server, _dir) = server_with_source("known");
        let err = server
            .roux_query(Parameters(query(Some("nope"))))
            .expect_err("unknown source should be rejected");
        assert!(err.message.contains("unknown source"), "got: {}", err.message);
        assert!(err.message.contains("known"), "should list available: {}", err.message);
    }

    #[test]
    fn known_source_is_accepted() {
        let (server, _dir) = server_with_source("known");
        assert!(server.roux_query(Parameters(query(Some("known")))).is_ok());
    }

    #[test]
    fn no_source_filter_is_accepted() {
        let (server, _dir) = server_with_source("known");
        assert!(server.roux_query(Parameters(query(None))).is_ok());
    }

    #[test]
    fn skeleton_resource_renders_matching_symbol() {
        let (server, _dir) = server_with_source("known");
        // Percent-encoded space to exercise URL decoding.
        let block = server
            .render_skeleton_uri("roux://skeleton/foo%20bar")
            .expect("skeleton read should succeed");
        // render_skeleton emits `- <qualified_name> (file:line)`.
        assert!(block.contains("known::foo"), "got: {block}");
        assert!(block.contains("lib.rs:1"), "got: {block}");
    }

    #[test]
    fn skeleton_resource_rejects_unknown_uri() {
        let (server, _dir) = server_with_source("known");
        let err = server
            .render_skeleton_uri("roux://bogus/foo")
            .expect_err("non-skeleton URI should be rejected");
        assert!(err.message.contains("unknown resource"), "got: {}", err.message);
    }

    #[test]
    fn skeleton_resource_rejects_empty_query() {
        let (server, _dir) = server_with_source("known");
        let err = server
            .render_skeleton_uri("roux://skeleton/%20")
            .expect_err("empty query should be rejected");
        assert!(err.message.contains("empty query"), "got: {}", err.message);
    }
}
