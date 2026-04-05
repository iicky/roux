#![allow(clippy::type_complexity)]

/// A test case: a natural language query and the expected symbol names in the result.
struct QueryCase {
    query: &'static str,
    /// Expected symbol names (any of these appearing in top-K is a hit)
    expected: &'static [&'static str],
    /// Whether this query relies on symbol names (robust) or docstrings (fragile)
    depends_on: QueryDep,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum QueryDep {
    /// Query terms match symbol names directly
    SymbolName,
    /// Query terms need docstrings or body text to match
    DocContent,
}

// ─── Self-benchmark: roux's own codebase ────────────────────────────

const ROUX_QUERIES: &[QueryCase] = &[
    QueryCase {
        query: "download a crate from crates.io",
        depends_on: QueryDep::SymbolName,
        expected: &["download_crate", "validate_crate_name"],
    },
    QueryCase {
        query: "search the graph for matching nodes",
        depends_on: QueryDep::SymbolName,
        expected: &["search", "GraphStore"],
    },
    QueryCase {
        query: "parse source code with tree-sitter",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_from_source", "extract_node", "extract_dir"],
    },
    QueryCase {
        query: "personalized pagerank ranking",
        depends_on: QueryDep::SymbolName,
        expected: &["personalized_pagerank", "rank_subgraph"],
    },
    QueryCase {
        query: "store nodes and edges in sqlite",
        depends_on: QueryDep::SymbolName,
        expected: &["upsert_source", "GraphStore"],
    },
    QueryCase {
        query: "extract Python functions and classes",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_python_node"],
    },
    QueryCase {
        query: "resolve unresolved references",
        depends_on: QueryDep::SymbolName,
        expected: &["resolve_references"],
    },
    QueryCase {
        query: "escape query string for fts matching",
        depends_on: QueryDep::SymbolName,
        expected: &["fts_query_escape", "tokenize_for_fts"],
    },
    QueryCase {
        query: "detect language from file extension",
        depends_on: QueryDep::SymbolName,
        expected: &["detect_language", "get_ts_language"],
    },
    QueryCase {
        query: "configuration and store path",
        depends_on: QueryDep::SymbolName,
        expected: &["resolve_store_path", "Config"],
    },
    QueryCase {
        query: "extract markdown documentation sections",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_markdown_doc", "flush_doc_section"],
    },
    QueryCase {
        query: "extract decorator edges from Python",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_decorator_edges", "decorates"],
    },
    QueryCase {
        query: "infer which tests cover which functions",
        depends_on: QueryDep::SymbolName,
        expected: &["infer_test_edges", "extract_tested_name"],
    },
    QueryCase {
        query: "detect function visibility public private",
        depends_on: QueryDep::SymbolName,
        expected: &["detect_visibility"],
    },
    QueryCase {
        query: "walk directory tree for source files",
        depends_on: QueryDep::SymbolName,
        expected: &["walk_dir"],
    },
    // ─── Additional queries for robustness ──────────────────
    QueryCase {
        query: "extract rust structs and enums",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_rust_node"],
    },
    QueryCase {
        query: "extract JS class and function nodes",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_js_node"],
    },
    QueryCase {
        query: "Go exported functions and methods",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_go_node"],
    },
    QueryCase {
        query: "backtick references in markdown",
        depends_on: QueryDep::DocContent,
        expected: &["extract_backtick_refs"],
    },
    QueryCase {
        query: "HTTP route handler detection",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_route_registrations", "routes"],
    },
    QueryCase {
        query: "raise throw error detection",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_raise_edges"],
    },
    QueryCase {
        query: "inheritance class extends parent",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_relationship_edges", "inherits"],
    },
    QueryCase {
        query: "blake3 hash of source text",
        depends_on: QueryDep::SymbolName,
        expected: &["content_hash", "build_body"],
    },
    QueryCase {
        query: "file node creation from path",
        depends_on: QueryDep::SymbolName,
        expected: &["make_file_node"],
    },
    QueryCase {
        query: "remove source delete from index",
        depends_on: QueryDep::SymbolName,
        expected: &["remove_source"],
    },
];

/// Compute Hit@K: fraction of queries where at least one expected symbol appears in top-K results.
fn hit_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
    let hits = results
        .iter()
        .filter(|(names, expected)| {
            names
                .iter()
                .take(k)
                .any(|name| expected.iter().any(|exp| name.contains(exp)))
        })
        .count();
    hits as f64 / results.len() as f64
}

/// Compute MRR (Mean Reciprocal Rank): average of 1/rank of first correct result.
fn mrr(results: &[(Vec<String>, &[&str])]) -> f64 {
    let sum: f64 = results
        .iter()
        .map(|(names, expected)| {
            for (i, name) in names.iter().enumerate() {
                if expected.iter().any(|exp| name.contains(exp)) {
                    return 1.0 / (i + 1) as f64;
                }
            }
            0.0
        })
        .sum();
    sum / results.len() as f64
}

/// Compute NDCG@K (Normalized Discounted Cumulative Gain).
fn ndcg_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
    let sum: f64 = results
        .iter()
        .map(|(names, expected)| {
            let mut dcg = 0.0f64;
            for (i, name) in names.iter().take(k).enumerate() {
                let rel = if expected.iter().any(|exp| name.contains(exp)) {
                    1.0
                } else {
                    0.0
                };
                dcg += rel / (i as f64 + 2.0).log2();
            }

            // Ideal DCG: all relevant results at top
            let n_relevant = expected.len().min(k);
            let mut idcg = 0.0f64;
            for i in 0..n_relevant {
                idcg += 1.0 / (i as f64 + 2.0).log2();
            }

            if idcg > 0.0 { dcg / idcg } else { 0.0 }
        })
        .sum();
    sum / results.len() as f64
}

/// Compute subgraph coherence: fraction of returned nodes that have at least one edge
/// to another returned node.
fn subgraph_coherence(node_ids: &[String], edges: &[(String, String)]) -> f64 {
    let id_set: std::collections::HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let connected = node_ids
        .iter()
        .filter(|id| {
            edges.iter().any(|(from, to)| {
                (from == *id && id_set.contains(to.as_str()))
                    || (to == *id && id_set.contains(from.as_str()))
            })
        })
        .count();
    if node_ids.is_empty() {
        0.0
    } else {
        connected as f64 / node_ids.len() as f64
    }
}

// ─── Self-benchmark test ────────────────────────────────────────────

#[test]
fn bench_self_retrieval() {
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;

    // Index roux's own source
    let store = GraphStore::open_in_memory().unwrap();
    let graph =
        extract::extract_dir(std::path::Path::new("src"), "roux", "dev", Some("rust")).unwrap();

    assert!(
        graph.nodes.len() > 50,
        "expected >50 nodes from roux source, got {}",
        graph.nodes.len()
    );

    store
        .upsert_source("roux", "dev", "rust", &graph.nodes, &graph.edges)
        .unwrap();

    // Run all queries
    let mut results: Vec<(Vec<String>, &[&str])> = Vec::new();
    let mut edge_data: Vec<(Vec<String>, Vec<(String, String)>)> = Vec::new();

    for case in ROUX_QUERIES {
        let result = store.search(case.query, 10).unwrap();
        let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();
        let node_ids: Vec<String> = result.nodes.iter().map(|n| n.id.clone()).collect();
        let edges: Vec<(String, String)> = result
            .edges
            .iter()
            .map(|e| (e.from_id.clone(), e.to_id.clone()))
            .collect();

        results.push((names, case.expected));
        edge_data.push((node_ids, edges));
    }

    // Compute metrics
    let h1 = hit_at_k(&results, 1);
    let h5 = hit_at_k(&results, 5);
    let h10 = hit_at_k(&results, 10);
    let mrr_score = mrr(&results);
    let ndcg = ndcg_at_k(&results, 10);

    let avg_coherence: f64 = edge_data
        .iter()
        .map(|(ids, edges)| subgraph_coherence(ids, edges))
        .sum::<f64>()
        / edge_data.len() as f64;

    // Print results
    eprintln!(
        "\n═══ roux self-benchmark ({} queries) ═══",
        ROUX_QUERIES.len()
    );
    eprintln!("  Hit@1:      {:.1}%", h1 * 100.0);
    eprintln!("  Hit@5:      {:.1}%", h5 * 100.0);
    eprintln!("  Hit@10:     {:.1}%", h10 * 100.0);
    eprintln!("  MRR:        {:.3}", mrr_score);
    eprintln!("  NDCG@10:    {:.3}", ndcg);
    eprintln!("  Coherence:  {:.1}%", avg_coherence * 100.0);

    // Print per-query results
    eprintln!("\n── per-query breakdown ──");
    for (i, case) in ROUX_QUERIES.iter().enumerate() {
        let (ref names, _) = results[i];
        let hit = case
            .expected
            .iter()
            .any(|exp| names.iter().take(10).any(|n| n.contains(exp)));
        let rank = names
            .iter()
            .position(|n| case.expected.iter().any(|exp| n.contains(exp)))
            .map(|r| r + 1);

        let status = if hit { "✓" } else { "✗" };
        let rank_str = rank
            .map(|r| format!("@{r}"))
            .unwrap_or_else(|| "miss".to_string());
        let dep = match case.depends_on {
            QueryDep::SymbolName => "name",
            QueryDep::DocContent => "doc ",
        };
        eprintln!("  {status} [{rank_str:>5}] ({dep}) {}", case.query);
    }

    // Breakdown by dependency type
    let name_results: Vec<_> = results
        .iter()
        .zip(ROUX_QUERIES.iter())
        .filter(|(_, c)| c.depends_on == QueryDep::SymbolName)
        .map(|(r, _)| r.clone())
        .collect();
    let doc_results: Vec<_> = results
        .iter()
        .zip(ROUX_QUERIES.iter())
        .filter(|(_, c)| c.depends_on == QueryDep::DocContent)
        .map(|(r, _)| r.clone())
        .collect();

    if !name_results.is_empty() {
        eprintln!("\n── by dependency ──");
        eprintln!("  Symbol-name queries ({}):", name_results.len());
        eprintln!(
            "    Hit@1: {:.1}%  MRR: {:.3}",
            hit_at_k(&name_results, 1) * 100.0,
            mrr(&name_results)
        );
        if !doc_results.is_empty() {
            eprintln!("  Doc-content queries ({}):", doc_results.len());
            eprintln!(
                "    Hit@1: {:.1}%  MRR: {:.3}",
                hit_at_k(&doc_results, 1) * 100.0,
                mrr(&doc_results)
            );
        }
    }

    // Hard gates — these block CI
    assert!(
        h10 >= 0.95,
        "HARD FAIL: Hit@10 regressed: {:.1}% (need ≥95%)",
        h10 * 100.0
    );
    assert!(
        mrr_score >= 0.65,
        "HARD FAIL: MRR regressed: {:.3} (need ≥0.65)",
        mrr_score
    );

    // Soft gates — warn but don't block
    if h1 < 0.55 {
        eprintln!(
            "  ⚠ WARNING: Hit@1 below target: {:.1}% (target ≥55%)",
            h1 * 100.0
        );
    }
    if h5 < 0.80 {
        eprintln!(
            "  ⚠ WARNING: Hit@5 below target: {:.1}% (target ≥80%)",
            h5 * 100.0
        );
    }
    if ndcg < 0.60 {
        eprintln!(
            "  ⚠ WARNING: NDCG@10 below target: {:.3} (target ≥0.60)",
            ndcg
        );
    }
    assert!(
        avg_coherence >= 0.3,
        "Subgraph coherence too low: {:.1}% (need ≥30%)",
        avg_coherence * 100.0
    );
}

/// Performance benchmark: measure ingestion and search latency.
#[test]
fn bench_performance() {
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;
    use std::time::Instant;

    // Index roux source
    let t0 = Instant::now();
    let graph =
        extract::extract_dir(std::path::Path::new("src"), "roux", "dev", Some("rust")).unwrap();
    let extract_ms = t0.elapsed().as_millis();

    let store = GraphStore::open_in_memory().unwrap();
    let t1 = Instant::now();
    store
        .upsert_source("roux", "dev", "rust", &graph.nodes, &graph.edges)
        .unwrap();
    let store_ms = t1.elapsed().as_millis();

    // Search latency (average over multiple queries)
    let queries = [
        "download crate",
        "search graph",
        "parse tree-sitter",
        "configuration",
        "walk directory",
    ];
    let t2 = Instant::now();
    for q in &queries {
        let _ = store.search(q, 10).unwrap();
    }
    let search_total_ms = t2.elapsed().as_millis();
    let search_avg_ms = search_total_ms as f64 / queries.len() as f64;

    eprintln!("\n═══ roux performance benchmark ═══");
    eprintln!("  Nodes:      {}", graph.nodes.len());
    eprintln!("  Edges:      {}", graph.edges.len());
    eprintln!("  Extract:    {}ms", extract_ms);
    eprintln!("  Store:      {}ms", store_ms);
    eprintln!(
        "  Search avg: {:.1}ms ({} queries)",
        search_avg_ms,
        queries.len()
    );
    eprintln!(
        "  Symbols/sec (extract): {:.0}",
        graph.nodes.len() as f64 / (extract_ms as f64 / 1000.0)
    );
    eprintln!(
        "  Symbols/sec (store):   {:.0}",
        graph.nodes.len() as f64 / (store_ms as f64 / 1000.0)
    );

    // Performance gates
    assert!(
        search_avg_ms < 50.0,
        "Search too slow: {:.1}ms (need <50ms)",
        search_avg_ms
    );
}

// ─── RRF vs Score Fusion A/B test ──────────────────────────────────

#[test]
fn bench_rrf_ab_test() {
    use roux_cli::graph::extract;
    use roux_cli::graph::rank::FusionMethod;
    use roux_cli::graph::store::GraphStore;

    let store = GraphStore::open_in_memory().unwrap();
    let graph =
        extract::extract_dir(std::path::Path::new("src"), "roux", "dev", Some("rust")).unwrap();
    store
        .upsert_source("roux", "dev", "rust", &graph.nodes, &graph.edges)
        .unwrap();

    let variants: Vec<(&str, FusionMethod, bool)> = vec![
        (
            "ScoreFusion (no desc rerank)",
            FusionMethod::ScoreFusion,
            false,
        ),
        ("ScoreFusion + desc rerank", FusionMethod::ScoreFusion, true),
        ("RRF (k=60)", FusionMethod::RRF, false),
    ];

    eprintln!(
        "\n═══ Fusion A/B test ({} queries) ═══\n",
        ROUX_QUERIES.len()
    );

    for (label, method, desc_rerank) in &variants {
        let mut results: Vec<(Vec<String>, &[&str])> = Vec::new();

        for case in ROUX_QUERIES {
            let result = store
                .search_with_opts(case.query, 10, *method, *desc_rerank)
                .unwrap();
            let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();
            results.push((names, case.expected));
        }

        let h1 = hit_at_k(&results, 1);
        let h5 = hit_at_k(&results, 5);
        let h10 = hit_at_k(&results, 10);
        let mrr_score = mrr(&results);
        let ndcg = ndcg_at_k(&results, 10);

        eprintln!("  {label}");
        eprintln!(
            "    Hit@1: {:.1}%  Hit@5: {:.1}%  Hit@10: {:.1}%  MRR: {:.3}  NDCG@10: {:.3}",
            h1 * 100.0,
            h5 * 100.0,
            h10 * 100.0,
            mrr_score,
            ndcg
        );

        for (i, case) in ROUX_QUERIES.iter().enumerate() {
            let (ref names, _) = results[i];
            let rank = names
                .iter()
                .position(|n| case.expected.iter().any(|exp| n.contains(exp)))
                .map(|r| r + 1);
            let status = if rank.is_some() { "✓" } else { "✗" };
            let rank_str = rank
                .map(|r| format!("@{r}"))
                .unwrap_or_else(|| "miss".to_string());
            eprintln!("    {status} [{rank_str:>5}] {}", case.query);
        }
        eprintln!();
    }
}

// ─── Express diagnostics ────────────────────────────────────────────

#[test]
#[ignore]
fn diag_express_misses() {
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;

    let store = GraphStore::open_in_memory().unwrap();
    let graph = extract::extract_dir(
        std::path::Path::new("/tmp/roux-sources/express"),
        "express",
        "dev",
        Some("javascript"),
    )
    .unwrap();
    // Kind distribution
    let mut kinds: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for n in &graph.nodes {
        *kinds.entry(n.kind.as_str()).or_default() += 1;
    }
    let mut sorted: Vec<_> = kinds.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!(
        "\n── express kind distribution ({} nodes) ──",
        graph.nodes.len()
    );
    for (kind, count) in &sorted {
        eprintln!(
            "  {:<15} {:>4} ({:.0}%)",
            kind,
            count,
            *count as f64 / graph.nodes.len() as f64 * 100.0
        );
    }

    store
        .upsert_source("express", "dev", "javascript", &graph.nodes, &graph.edges)
        .unwrap();

    let queries = [
        ("send file in response", &["sendfile", "onfile"][..]),
        ("cookie handling", &["getCookies", "getCookie"][..]),
    ];

    for (query, expected) in &queries {
        let result = store.search(query, 10).unwrap();
        eprintln!("\n── query: \"{query}\" (expect: {expected:?}) ──");
        for (i, node) in result.nodes.iter().enumerate() {
            let score = result.scores.get(&node.id).copied().unwrap_or(0.0);
            let is_hit = expected.iter().any(|e| node.name.contains(e));
            let marker = if is_hit { " ◀" } else { "" };
            let desc = node.description.as_deref().unwrap_or("");
            eprintln!(
                "  {:>2}. [{:.4}] {} ({}) — {}{marker}",
                i + 1,
                score,
                node.name,
                node.kind,
                &desc[..desc.len().min(80)]
            );
        }
    }
}

// ─── Adversarial query suite ────────────────────────────────────────

/// Adversarial queries designed to probe system boundaries.
/// These intentionally avoid symbol-name overlap to test semantic retrieval limits.
const ADVERSARIAL_ROUX: &[QueryCase] = &[
    // Synonym queries — natural language, not code names
    QueryCase {
        query: "authenticate user credentials",
        depends_on: QueryDep::DocContent,
        expected: &["detect_visibility", "resolve_references"], // weak match expected
    },
    QueryCase {
        query: "serialize data to disk",
        depends_on: QueryDep::DocContent,
        expected: &["upsert_source", "GraphStore"],
    },
    QueryCase {
        query: "find all symbols in a file",
        depends_on: QueryDep::DocContent,
        expected: &["extract_from_source", "extract_dir"],
    },
    // Behavioral queries — "what does X" with no symbol name overlap
    QueryCase {
        query: "what happens when a file is not found",
        depends_on: QueryDep::DocContent,
        expected: &["walk_dir", "extract_dir"],
    },
    QueryCase {
        query: "where does indexing start",
        depends_on: QueryDep::DocContent,
        expected: &["extract_dir", "upsert_source"],
    },
    QueryCase {
        query: "how are search results ranked",
        depends_on: QueryDep::DocContent,
        expected: &["rank_subgraph", "personalized_pagerank", "search"],
    },
    // Typos and partial names
    QueryCase {
        query: "extrat_dir",
        depends_on: QueryDep::SymbolName,
        expected: &["extract_dir"],
    },
    QueryCase {
        query: "walkdir",
        depends_on: QueryDep::SymbolName,
        expected: &["walk_dir"],
    },
    QueryCase {
        query: "graphstore",
        depends_on: QueryDep::SymbolName,
        expected: &["GraphStore"],
    },
    // Multi-concept queries
    QueryCase {
        query: "parse source and store in database",
        depends_on: QueryDep::DocContent,
        expected: &["extract_from_source", "upsert_source", "GraphStore"],
    },
    QueryCase {
        query: "tree sitter language detection",
        depends_on: QueryDep::SymbolName,
        expected: &["get_ts_language", "detect_language"],
    },
    // Cross-cutting concern
    QueryCase {
        query: "everything that reads from sqlite",
        depends_on: QueryDep::DocContent,
        expected: &["GraphStore", "search", "fetch_nodes"],
    },
    // Vague/ambiguous
    QueryCase {
        query: "the main entry point",
        depends_on: QueryDep::DocContent,
        expected: &["main", "run"],
    },
    QueryCase {
        query: "configuration",
        depends_on: QueryDep::SymbolName,
        expected: &["Config", "resolve_store_path"],
    },
    // Negation-style (system can't handle, but let's see what happens)
    QueryCase {
        query: "not extraction but storage",
        depends_on: QueryDep::DocContent,
        expected: &["GraphStore", "upsert_source"],
    },
];

const ADVERSARIAL_FLASK: &[QueryCase] = &[
    // Synonym
    QueryCase {
        query: "start the web server",
        depends_on: QueryDep::DocContent,
        expected: &["run", "Flask"],
    },
    QueryCase {
        query: "authenticate user session",
        depends_on: QueryDep::DocContent,
        expected: &["SessionInterface", "open_session"],
    },
    // Behavioral
    QueryCase {
        query: "what happens on 404",
        depends_on: QueryDep::DocContent,
        expected: &["handle_exception", "handle_http_exception"],
    },
    QueryCase {
        query: "where does request dispatching begin",
        depends_on: QueryDep::DocContent,
        expected: &["dispatch_request", "wsgi_app", "full_dispatch_request"],
    },
    // Typo
    QueryCase {
        query: "blueprnt register",
        depends_on: QueryDep::SymbolName,
        expected: &["register_blueprint", "register", "Blueprint"],
    },
    // Multi-concept
    QueryCase {
        query: "render template with context variables",
        depends_on: QueryDep::SymbolName,
        expected: &["render_template", "render"],
    },
    // Cross-cutting
    QueryCase {
        query: "everything that modifies the response",
        depends_on: QueryDep::DocContent,
        expected: &["make_response", "Response", "process_response"],
    },
    // Vague
    QueryCase {
        query: "the app object",
        depends_on: QueryDep::DocContent,
        expected: &["Flask", "App"],
    },
];

const ADVERSARIAL_EXPRESS: &[QueryCase] = &[
    // Synonym
    QueryCase {
        query: "start listening on a port",
        depends_on: QueryDep::DocContent,
        expected: &["listen", "createApplication"],
    },
    QueryCase {
        query: "authenticate request",
        depends_on: QueryDep::DocContent,
        expected: &["use", "handle"],
    },
    // Behavioral
    QueryCase {
        query: "what handles 404 errors",
        depends_on: QueryDep::DocContent,
        expected: &["finalhandler", "onerror", "logerror"],
    },
    QueryCase {
        query: "where does routing start",
        depends_on: QueryDep::DocContent,
        expected: &["handle", "route", "dispatch"],
    },
    // Typo
    QueryCase {
        query: "middlewre chain",
        depends_on: QueryDep::SymbolName,
        expected: &["use", "handle", "next"],
    },
    // Multi-concept
    QueryCase {
        query: "parse query string and set content type",
        depends_on: QueryDep::DocContent,
        expected: &["query", "contentType", "type"],
    },
];

#[test]
fn bench_adversarial_self() {
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;

    let store = GraphStore::open_in_memory().unwrap();
    let graph =
        extract::extract_dir(std::path::Path::new("src"), "roux", "dev", Some("rust")).unwrap();
    store
        .upsert_source("roux", "dev", "rust", &graph.nodes, &graph.edges)
        .unwrap();

    let (h10, mrr_score) = run_adversarial("roux", &store, ADVERSARIAL_ROUX);

    // Regression gates — lock in current adversarial floor
    assert!(
        h10 >= 0.55,
        "Adversarial Hit@10 regressed: {:.1}% (need ≥55%)",
        h10 * 100.0
    );
    assert!(
        mrr_score >= 0.25,
        "Adversarial MRR regressed: {:.3} (need ≥0.25)",
        mrr_score
    );
}

#[test]
#[ignore]
fn bench_adversarial_multi() {
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;

    let adversarial_repos: &[(&str, &str, &str, &[QueryCase])] = &[
        (
            "flask",
            "/tmp/roux-sources/flask",
            "python",
            ADVERSARIAL_FLASK,
        ),
        (
            "express",
            "/tmp/roux-sources/express",
            "javascript",
            ADVERSARIAL_EXPRESS,
        ),
    ];

    for (name, path, lang, queries) in adversarial_repos {
        let p = std::path::Path::new(path);
        if !p.exists() {
            eprintln!("  SKIP {name}");
            continue;
        }
        let store = GraphStore::open_in_memory().unwrap();
        let graph = extract::extract_dir(p, name, "dev", Some(lang)).unwrap();
        store
            .upsert_source(name, "dev", lang, &graph.nodes, &graph.edges)
            .unwrap();
        run_adversarial(name, &store, queries);
    }
}

fn run_adversarial(
    name: &str,
    store: &roux_cli::graph::store::GraphStore,
    queries: &[QueryCase],
) -> (f64, f64) {
    let mut results: Vec<(Vec<String>, &[&str])> = Vec::new();

    eprintln!("\n═══ adversarial: {name} ({} queries) ═══", queries.len());

    for case in queries {
        let result = store.search(case.query, 10).unwrap();
        let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();

        let rank = names
            .iter()
            .position(|n| case.expected.iter().any(|exp| n.contains(exp)))
            .map(|r| r + 1);
        let status = if rank.is_some() { "✓" } else { "✗" };
        let rank_str = rank
            .map(|r| format!("@{r}"))
            .unwrap_or_else(|| "miss".to_string());
        let dep = match case.depends_on {
            QueryDep::SymbolName => "name",
            QueryDep::DocContent => "doc ",
        };
        eprintln!("  {status} [{rank_str:>5}] ({dep}) {}", case.query);

        // Show top-3 for misses
        if rank.is_none() || rank.unwrap() > 5 {
            let top3: Vec<String> = result
                .nodes
                .iter()
                .take(3)
                .map(|n| format!("{}({})", n.name, n.kind))
                .collect();
            eprintln!("         got: {}", top3.join(", "));
        }

        results.push((names, case.expected));
    }

    let h1 = hit_at_k(&results, 1);
    let h5 = hit_at_k(&results, 5);
    let h10 = hit_at_k(&results, 10);
    let mrr_score = mrr(&results);
    let ndcg = ndcg_at_k(&results, 10);

    // Breakdown by dep type
    let name_r: Vec<_> = results
        .iter()
        .zip(queries)
        .filter(|(_, c)| c.depends_on == QueryDep::SymbolName)
        .map(|(r, _)| r.clone())
        .collect();
    let doc_r: Vec<_> = results
        .iter()
        .zip(queries)
        .filter(|(_, c)| c.depends_on == QueryDep::DocContent)
        .map(|(r, _)| r.clone())
        .collect();

    eprintln!(
        "\n  ALL        Hit@1:{:>5.1}%  Hit@5:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}  NDCG:{:.3}",
        h1 * 100.0,
        h5 * 100.0,
        h10 * 100.0,
        mrr_score,
        ndcg
    );
    if !name_r.is_empty() {
        eprintln!(
            "  name-dep   Hit@1:{:>5.1}%  MRR:{:.3} ({}q)",
            hit_at_k(&name_r, 1) * 100.0,
            mrr(&name_r),
            name_r.len()
        );
    }
    if !doc_r.is_empty() {
        eprintln!(
            "  doc-dep    Hit@1:{:>5.1}%  MRR:{:.3} ({}q)",
            hit_at_k(&doc_r, 1) * 100.0,
            mrr(&doc_r),
            doc_r.len()
        );
    }
    eprintln!();

    (h10, mrr_score)
}

// ─── Multi-repo benchmark (requires cloned repos) ───────────────────

struct RepoBench {
    name: &'static str,
    path: &'static str,
    language: &'static str,
    queries: &'static [QueryCase],
}

const MULTI_REPO: &[RepoBench] = &[
    RepoBench {
        name: "flask",
        path: "/tmp/roux-sources/flask",
        language: "python",
        queries: &[
            QueryCase {
                query: "create logger for application",
                depends_on: QueryDep::SymbolName,
                expected: &["create_logger"],
            },
            QueryCase {
                query: "register blueprint with app",
                depends_on: QueryDep::SymbolName,
                expected: &["register_blueprint", "register"],
            },
            QueryCase {
                query: "add url routing rule",
                depends_on: QueryDep::SymbolName,
                expected: &["add_url_rule"],
            },
            QueryCase {
                query: "session cookie interface",
                depends_on: QueryDep::SymbolName,
                expected: &["SessionInterface", "get_cookie_name"],
            },
            QueryCase {
                query: "send file response",
                depends_on: QueryDep::SymbolName,
                expected: &["sendfile", "send_file"],
            },
            QueryCase {
                query: "template rendering",
                depends_on: QueryDep::SymbolName,
                expected: &["render_template", "render"],
            },
            QueryCase {
                query: "handle_exception app error",
                depends_on: QueryDep::SymbolName,
                expected: &["handle_exception", "AppError"],
            },
            QueryCase {
                query: "request context",
                depends_on: QueryDep::SymbolName,
                expected: &["AppContext", "RequestContext"],
            },
        ],
    },
    RepoBench {
        name: "express",
        path: "/tmp/roux-sources/express",
        language: "javascript",
        queries: &[
            QueryCase {
                query: "create express application",
                depends_on: QueryDep::SymbolName,
                expected: &["createApplication"],
            },
            QueryCase {
                query: "send file in response",
                depends_on: QueryDep::SymbolName,
                expected: &["sendfile", "onfile"],
            },
            QueryCase {
                query: "parse error handling",
                depends_on: QueryDep::SymbolName,
                expected: &["parseError", "onerror"],
            },
            QueryCase {
                query: "cookie handling",
                depends_on: QueryDep::SymbolName,
                expected: &["getCookies", "getCookie"],
            },
        ],
    },
    RepoBench {
        name: "ripgrep",
        path: "/tmp/roux-sources/ripgrep",
        language: "rust",
        queries: &[
            QueryCase {
                query: "search_reader searcher",
                depends_on: QueryDep::SymbolName,
                expected: &["search_reader", "Searcher"],
            },
            QueryCase {
                query: "detect_binary binary byte offset",
                depends_on: QueryDep::SymbolName,
                expected: &["detect_binary", "binary_data", "binary_byte_offset"],
            },
            QueryCase {
                query: "GlobBuilder glob pattern matching",
                depends_on: QueryDep::SymbolName,
                expected: &["GlobBuilder", "Candidate", "glob"],
            },
            QueryCase {
                query: "before_context_by_line after_context",
                depends_on: QueryDep::SymbolName,
                expected: &["before_context_by_line", "after_context_by_line"],
            },
            QueryCase {
                query: "LineBufferReader line_buffer",
                depends_on: QueryDep::SymbolName,
                expected: &["LineBufferReader", "LineIter"],
            },
            QueryCase {
                query: "PathPrinter CounterWriter JSON printer",
                depends_on: QueryDep::SymbolName,
                expected: &["PathPrinter", "CounterWriter", "JSON"],
            },
            QueryCase {
                query: "LowArgs from_low_args flag parsing",
                depends_on: QueryDep::SymbolName,
                expected: &["LowArgs", "from_low_args", "ParseResult"],
            },
            QueryCase {
                query: "walk_builder visit directory ignore",
                depends_on: QueryDep::SymbolName,
                expected: &["walk_builder", "visit", "build"],
            },
        ],
    },
];

#[test]
#[ignore] // requires cloned repos at /tmp/roux-sources/
fn bench_multi_repo() {
    use roux_cli::graph::extract;
    use roux_cli::graph::rank::FusionMethod;
    use roux_cli::graph::store::GraphStore;
    use std::time::Instant;

    eprintln!("\n═══ multi-repo desc rerank A/B ═══\n");

    let variants: &[(&str, bool)] = &[
        ("baseline (no desc rerank)", false),
        ("+ desc rerank", true),
    ];

    // Index all repos once, store handles for reuse
    struct IndexedRepo<'a> {
        bench: &'a RepoBench,
        store: GraphStore,
    }

    let mut repos: Vec<IndexedRepo> = Vec::new();
    for repo in MULTI_REPO {
        let path = std::path::Path::new(repo.path);
        if !path.exists() {
            eprintln!("  SKIP {} (not found at {})", repo.name, repo.path);
            continue;
        }

        let store = GraphStore::open_in_memory().unwrap();
        let t0 = Instant::now();
        let graph = extract::extract_dir(path, repo.name, "dev", Some(repo.language)).unwrap();
        let extract_ms = t0.elapsed().as_millis();

        let node_count = graph.nodes.len();
        let edge_count = graph.edges.len();
        store
            .upsert_source(repo.name, "dev", repo.language, &graph.nodes, &graph.edges)
            .unwrap();

        eprintln!(
            "  {:<12} {:>5} nodes  {:>5} edges  {:>4}ms extract",
            repo.name, node_count, edge_count, extract_ms,
        );

        repos.push(IndexedRepo { bench: repo, store });
    }

    if repos.is_empty() {
        eprintln!("  No repos found — skipping");
        return;
    }

    eprintln!();

    for (label, desc_rerank) in variants {
        eprintln!("── {label} ──");

        let mut all_results: Vec<(Vec<String>, &[&str])> = Vec::new();

        for indexed in &repos {
            let mut repo_results: Vec<(Vec<String>, &[&str])> = Vec::new();

            for case in indexed.bench.queries {
                let result = indexed
                    .store
                    .search_with_opts(case.query, 10, FusionMethod::ScoreFusion, *desc_rerank)
                    .unwrap();
                let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();
                repo_results.push((names, case.expected));
            }

            let h1 = hit_at_k(&repo_results, 1);
            let h5 = hit_at_k(&repo_results, 5);
            let h10 = hit_at_k(&repo_results, 10);
            let repo_mrr = mrr(&repo_results);

            eprintln!(
                "  {:<12} Hit@1:{:>5.1}%  Hit@5:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}",
                indexed.bench.name,
                h1 * 100.0,
                h5 * 100.0,
                h10 * 100.0,
                repo_mrr,
            );

            for (i, case) in indexed.bench.queries.iter().enumerate() {
                let (ref names, _) = repo_results[i];
                let rank = names
                    .iter()
                    .position(|n| case.expected.iter().any(|exp| n.contains(exp)))
                    .map(|r| r + 1);
                let status = if rank.is_some() { "✓" } else { "✗" };
                let rank_str = rank
                    .map(|r| format!("@{r}"))
                    .unwrap_or_else(|| "miss".to_string());
                eprintln!("    {status} [{rank_str:>5}] {}", case.query);
            }

            all_results.extend(repo_results);
        }

        let total_h1 = hit_at_k(&all_results, 1);
        let total_h5 = hit_at_k(&all_results, 5);
        let total_h10 = hit_at_k(&all_results, 10);
        let total_mrr = mrr(&all_results);
        let total_ndcg = ndcg_at_k(&all_results, 10);

        eprintln!(
            "  TOTAL      Hit@1:{:>5.1}%  Hit@5:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}  NDCG:{:.3}",
            total_h1 * 100.0,
            total_h5 * 100.0,
            total_h10 * 100.0,
            total_mrr,
            total_ndcg,
        );
        eprintln!();
    }
}
