//! MCP (Model Context Protocol) stdio server. Wraps existing CLI query/list
//! logic so AI agents (Claude Code, Cursor, etc.) can call roux directly
//! without shell prompts, eliminating per-call permission overhead.

use std::path::PathBuf;

use anyhow::Result;
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Deserialize;

use crate::cli::{check_source_status, list_rows_to_json, search_result_to_json};
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
        description = "Search the roux code index. Returns matched symbols plus their graph neighborhood (callers, callees, parent types) — typically more useful than a flat list. Prefer this over grep for code-exploration questions; results carry file path, line, signature, and rendered doc.\n\nRanking is BM25 over symbol names, signatures, and qualified paths, so queries that share tokens with the symbol name work best. For conceptual or behavioral questions (\"how does X work\", \"where is Y handled\") the code rarely uses the user's words — so reformulate into the jargon and identifiers the code likely uses and pass them together in the `queries` array (RRF-fused in one call). E.g. for \"limit sudden jolts when speed changes\" pass queries=[\"jerk limit\", \"junction deviation\", \"M205\"]. Measured to recover behavioral hits a single literal query misses. Each call is cheap; reformulating beats one broad query."
    )]
    fn roux_query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.open_store()?;
        let top = args.top.unwrap_or(5);
        let mut queries = vec![args.query];
        if let Some(extra) = args.queries {
            queries.extend(extra);
        }
        let result = store
            .search_multi(&queries, top, args.source.as_deref())
            .map_err(|e| McpError::internal_error(format!("search: {e}"), None))?;
        let json = search_result_to_json(&result);
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

#[tool_handler]
impl ServerHandler for RouxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Graph-native code retrieval for AI agents. Use roux_query for symbol search; \
             results include 2-hop graph neighborhood (callers, callees, parent types). \
             roux_list shows what's indexed; roux_status reports index health.\n\n\
             Ranking is name-biased BM25 — for behavioral questions (\"how does X work\"), \
             follow up with 2–3 likely symbol-name variants in separate calls rather than \
             one verbose query."
                    .to_string(),
            )
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
