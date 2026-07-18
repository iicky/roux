//! MCP (Model Context Protocol) stdio server. Wraps existing CLI query/list
//! logic so AI agents (Claude Code, Cursor, etc.) can call roux directly
//! without shell prompts, eliminating per-call permission overhead.

use std::path::PathBuf;

use anyhow::Result;
use percent_encoding::percent_decode_str;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
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

use crate::cli::{render_compact, render_skeleton, search_result_to_json};
use crate::graph::store::GraphStore;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct QueryArgs {
    /// Search query — natural language or keywords. Matches symbol names,
    /// signatures, docstrings, and bodies.
    pub query: String,
    /// Extra query variants, RRF-fused with `query` in one call. Reformulate the
    /// question into the identifiers/jargon the code likely uses — e.g. for
    /// "limit sudden jolts" pass ["jerk limit", "junction deviation", "M205"].
    /// The main lever for conceptual questions where the code's words differ
    /// from the user's.
    #[serde(default)]
    pub queries: Option<Vec<String>>,
    /// Max hits to return. Defaults to 5.
    #[serde(default)]
    pub top: Option<usize>,
    /// Restrict search to a single indexed source.
    #[serde(default)]
    pub source: Option<String>,
    /// Return the full JSON graph (ids, scores, edges, bodies) instead of the
    /// default compact text block. Defaults false — prefer the default unless
    /// you need ids/scores/edges.
    #[serde(default)]
    pub json: Option<bool>,
}

#[derive(Clone)]
pub struct RouxServer {
    store_path: PathBuf,
}

#[tool_router]
impl RouxServer {
    pub fn new(store_path: PathBuf) -> Self {
        Self { store_path }
    }

    fn open_store(&self) -> Result<GraphStore, McpError> {
        GraphStore::open(&self.store_path)
            .map_err(|e| McpError::internal_error(format!("open index: {e}"), None))
    }

    #[tool(
        description = "Search the code index; returns matched symbols plus their graph neighborhood (callers, callees, parent types) as a compact text block (file:line, symbol, signature, and a `near:` line of neighbors). Prefer over grep for code-exploration questions. Ranking is name-biased BM25, so for conceptual/behavioral questions (\"how does X work\") pass several likely symbol-name variants in `queries` (RRF-fused in one call) rather than one broad query. Set `json:true` for the full graph (ids, scores, edges, bodies)."
    )]
    fn roux_query(
        &self,
        Parameters(args): Parameters<QueryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.open_store()?;
        // Bound `top` so a caller can't request an unreasonable result set.
        const MAX_TOP: usize = 1000;
        let top = args
            .top
            .unwrap_or(crate::cli::DEFAULT_TOP_K)
            .clamp(1, MAX_TOP);

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
        // Staleness guard: surface files changed since indexing so a live agent
        // doesn't trust stale locations, in both output modes.
        let stale = crate::cli::stale_sources_for_result(&store, &result);

        // Default to a compact one-line-per-hit block (file:line symbol — sig,
        // plus a `near:` line of graph neighbors): a fraction of the JSON payload
        // while keeping the neighborhood that sets roux apart from grep. Opt into
        // the full graph (ids, scores, edges, bodies) with `json:true`.
        if !args.json.unwrap_or(false) {
            let mut block = render_compact(&result);
            if !stale.is_empty() {
                block = format!("{}\n{block}", crate::cli::format_stale_warning(&stale));
            }
            return Ok(CallToolResult::success(vec![Content::text(block)]));
        }

        let mut json = search_result_to_json(&result);
        if !stale.is_empty()
            && let Some(obj) = json.as_object_mut()
        {
            obj.insert("stale".into(), crate::cli::stale_to_json(&stale));
        }
        let body = serde_json::to_string_pretty(&json)
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
            "Graph-native code retrieval for AI agents. Use roux_query to search; \
             results are a compact block of matched symbols plus their graph \
             neighborhood (callers, callees, parent types). Ranking is name-biased \
             BM25 — for behavioral questions (\"how does X work\"), pass 2–3 likely \
             symbol-name variants in the `queries` array rather than one broad \
             query.\n\n\
             For a one-shot, prompt-cacheable context block, read the \
             `roux://skeleton/{query}` resource ONCE at the start of a task instead \
             of calling roux_query every turn."
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
                 calling roux_query per turn — the block is deterministic and \
                 prompt-cacheable, so it is read once and reused cheaply on later \
                 turns. {query} is a natural-language or keyword search string, URL-encoded."
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
            store
                .upsert_source(name, "1.0", "rust", &[node], &[])
                .unwrap();
        }
        (RouxServer::new(path), dir)
    }

    fn query(source: Option<&str>) -> QueryArgs {
        QueryArgs {
            query: "foo".into(),
            queries: None,
            top: None,
            source: source.map(str::to_string),
            json: None,
        }
    }

    #[test]
    fn unknown_source_is_rejected() {
        let (server, _dir) = server_with_source("known");
        let err = server
            .roux_query(Parameters(query(Some("nope"))))
            .expect_err("unknown source should be rejected");
        assert!(
            err.message.contains("unknown source"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("known"),
            "should list available: {}",
            err.message
        );
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

    fn tool_text(res: CallToolResult) -> String {
        res.content[0].as_text().unwrap().text.clone()
    }

    #[test]
    fn roux_query_defaults_to_compact_text() {
        let (server, _d) = server_with_source("known");
        let res = server.roux_query(Parameters(query(None))).unwrap();
        let text = tool_text(res);
        // Default output must be the compact text block, not the JSON graph.
        assert!(text.contains(" — "), "got: {text}");
        assert!(text.contains("known::foo"), "got: {text}");
        assert!(text.contains("lib.rs:1"), "got: {text}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.as_object().cloned())
                .is_none(),
            "got: {text}"
        );
    }

    #[test]
    fn roux_query_json_flag_returns_full_graph() {
        let (server, _d) = server_with_source("known");
        let args = QueryArgs {
            query: "foo".into(),
            queries: None,
            top: None,
            source: None,
            json: Some(true),
        };
        let res = server.roux_query(Parameters(args)).unwrap();
        let text = tool_text(res);
        assert!(
            serde_json::from_str::<serde_json::Value>(&text)
                .unwrap()
                .is_object(),
            "got: {text}"
        );
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
        assert!(
            err.message.contains("unknown resource"),
            "got: {}",
            err.message
        );
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
