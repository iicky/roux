/// Retrieval quality benchmark suite for roux.
///
/// Measures Hit@K, MRR, and NDCG@10 against known-answer queries.
/// Run with: cargo test --test retrieval_bench
/// Or for the full suite: cargo test --test retrieval_bench -- --ignored

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
        search_avg_ms < 10.0,
        "Search too slow: {:.1}ms (need <10ms)",
        search_avg_ms
    );
}
