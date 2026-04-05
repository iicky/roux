/// Embedding benchmark: measures whether vector search improves retrieval
/// on queries that BM25 misses (the adversarial/developer queries).
///
/// Run with: cargo test --test bench_embeddings -- --ignored --nocapture

struct EmbedQuery {
    query: &'static str,
    expected: &'static [&'static str],
}

// These are the queries that BM25 MISSES — the semantic gap queries
const SEMANTIC_GAP_QUERIES: &[EmbedQuery] = &[
    // From adversarial suite — all misses on BM25
    EmbedQuery {
        query: "how does line buffering work during search",
        expected: &["LineBuffer", "LineIter"],
    },
    EmbedQuery {
        query: "memory mapped file search",
        expected: &["MmapChoice", "mmap"],
    },
    EmbedQuery {
        query: "where does indexing start",
        expected: &["extract_dir", "upsert_source"],
    },
    EmbedQuery {
        query: "how are search results ranked",
        expected: &["rank_subgraph", "personalized_pagerank", "search"],
    },
    EmbedQuery {
        query: "serialize data to disk",
        expected: &["upsert_source", "GraphStore"],
    },
    EmbedQuery {
        query: "find all symbols in a file",
        expected: &["extract_from_source", "extract_dir"],
    },
    EmbedQuery {
        query: "the main entry point",
        expected: &["main", "run"],
    },
    EmbedQuery {
        query: "parse source and store in database",
        expected: &["extract_from_source", "upsert_source", "GraphStore"],
    },
];

fn hit_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
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

fn mrr(results: &[(Vec<String>, &[&str])]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
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

#[test]
#[ignore]
fn bench_embedding_value() {
    use roux_cli::embed::Embedder;
    use roux_cli::embed::candle::CandleEmbedder;
    use roux_cli::graph::extract;
    use roux_cli::graph::store::GraphStore;
    use std::time::Instant;

    // Index roux source
    let store = GraphStore::open_in_memory().unwrap();
    let graph =
        extract::extract_dir(std::path::Path::new("src"), "roux", "dev", Some("rust")).unwrap();
    store
        .upsert_source("roux", "dev", "rust", &graph.nodes, &graph.edges)
        .unwrap();

    eprintln!("\n═══ Embedding Value Benchmark ═══\n");
    eprintln!("  {} nodes, {} edges", graph.nodes.len(), graph.edges.len());

    // Generate embeddings
    eprintln!("  Loading model...");
    let t0 = Instant::now();
    // Try different models — change this to test alternatives
    let model_id = std::env::var("ROUX_MODEL")
        .unwrap_or_else(|_| "sentence-transformers/all-MiniLM-L6-v2".to_string());
    eprintln!("  Model: {model_id}");
    let embedder = CandleEmbedder::from_pretrained(&model_id).expect("failed to load model");
    eprintln!("  Model loaded in {}ms", t0.elapsed().as_millis());

    let node_texts = store.get_node_descriptions("roux").unwrap();
    let texts: Vec<&str> = node_texts.iter().map(|(_, t)| t.as_str()).collect();

    eprintln!("  Embedding {} nodes...", texts.len());
    let t1 = Instant::now();
    let vectors = embedder.embed_passages(&texts).unwrap();
    let embed_ms = t1.elapsed().as_millis();
    eprintln!(
        "  Embedded in {}ms ({:.1}ms/node)",
        embed_ms,
        embed_ms as f64 / texts.len() as f64
    );

    let pairs: Vec<(String, Vec<f32>)> = node_texts
        .into_iter()
        .zip(vectors)
        .map(|((id, _), vec)| (id, vec))
        .collect();
    store.store_vectors(&pairs).unwrap();

    // === BM25-only ===
    let mut bm25_results: Vec<(Vec<String>, &[&str])> = Vec::new();
    for q in SEMANTIC_GAP_QUERIES {
        let result = store.search(q.query, 10).unwrap();
        let names: Vec<String> = result.nodes.iter().map(|n| n.name.clone()).collect();
        bm25_results.push((names, q.expected));
    }

    // === Vector-only ===
    let mut vec_results: Vec<(Vec<String>, &[&str])> = Vec::new();
    for q in SEMANTIC_GAP_QUERIES {
        let query_vec = embedder.embed_query(q.query).unwrap();
        let scored = store.vector_search(&query_vec, 10).unwrap();
        let ids: Vec<String> = scored.iter().map(|(id, _)| id.clone()).collect();
        let nodes = store.fetch_nodes(&ids).unwrap();
        let names: Vec<String> = nodes.iter().map(|n| n.name.clone()).collect();
        vec_results.push((names, q.expected));
    }

    // === Hybrid: merge BM25 + vector scores ===
    let mut hybrid_results: Vec<(Vec<String>, &[&str])> = Vec::new();
    for q in SEMANTIC_GAP_QUERIES {
        let bm25 = store.search(q.query, 20).unwrap();
        let query_vec = embedder.embed_query(q.query).unwrap();
        let vec_scored = store.vector_search(&query_vec, 20).unwrap();

        // Merge: normalize both to [0,1], combine with equal weight
        let bm25_max = bm25
            .scores
            .values()
            .cloned()
            .fold(0.0f64, f64::max)
            .max(1e-10);
        let vec_max = vec_scored
            .first()
            .map(|(_, s)| *s)
            .unwrap_or(1.0)
            .max(1e-10);

        let mut combined: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

        for (id, score) in &bm25.scores {
            *combined.entry(id.clone()).or_default() += score / bm25_max * 0.5;
        }
        for (id, score) in &vec_scored {
            *combined.entry(id.clone()).or_default() += score / vec_max * 0.5;
        }

        let mut sorted: Vec<(String, f64)> = combined.into_iter().collect();
        sorted.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let top_ids: Vec<String> = sorted.iter().take(10).map(|(id, _)| id.clone()).collect();
        let nodes = store.fetch_nodes(&top_ids).unwrap();

        // Preserve ranking order
        let id_to_name: std::collections::HashMap<&str, &str> = nodes
            .iter()
            .map(|n| (n.id.as_str(), n.name.as_str()))
            .collect();
        let names: Vec<String> = top_ids
            .iter()
            .filter_map(|id| id_to_name.get(id.as_str()).map(|n| n.to_string()))
            .collect();
        hybrid_results.push((names, q.expected));
    }

    // Print comparison
    eprintln!("\n── per-query comparison ──");
    for (i, q) in SEMANTIC_GAP_QUERIES.iter().enumerate() {
        let bm25_rank = bm25_results[i]
            .0
            .iter()
            .position(|n| q.expected.iter().any(|e| n.contains(e)))
            .map(|r| format!("@{}", r + 1));
        let vec_rank = vec_results[i]
            .0
            .iter()
            .position(|n| q.expected.iter().any(|e| n.contains(e)))
            .map(|r| format!("@{}", r + 1));
        let hyb_rank = hybrid_results[i]
            .0
            .iter()
            .position(|n| q.expected.iter().any(|e| n.contains(e)))
            .map(|r| format!("@{}", r + 1));

        eprintln!(
            "  BM25:{:>6}  VEC:{:>6}  HYB:{:>6}  {}",
            bm25_rank.as_deref().unwrap_or("miss"),
            vec_rank.as_deref().unwrap_or("miss"),
            hyb_rank.as_deref().unwrap_or("miss"),
            q.query,
        );
    }

    let bm25_h1 = hit_at_k(&bm25_results, 1);
    let bm25_h10 = hit_at_k(&bm25_results, 10);
    let bm25_mrr = mrr(&bm25_results);

    let vec_h1 = hit_at_k(&vec_results, 1);
    let vec_h10 = hit_at_k(&vec_results, 10);
    let vec_mrr = mrr(&vec_results);

    let hyb_h1 = hit_at_k(&hybrid_results, 1);
    let hyb_h10 = hit_at_k(&hybrid_results, 10);
    let hyb_mrr = mrr(&hybrid_results);

    eprintln!("\n── aggregate ──");
    eprintln!(
        "  BM25-only   Hit@1:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}",
        bm25_h1 * 100.0,
        bm25_h10 * 100.0,
        bm25_mrr,
    );
    eprintln!(
        "  Vector-only Hit@1:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}",
        vec_h1 * 100.0,
        vec_h10 * 100.0,
        vec_mrr,
    );
    eprintln!(
        "  Hybrid      Hit@1:{:>5.1}%  Hit@10:{:>5.1}%  MRR:{:.3}",
        hyb_h1 * 100.0,
        hyb_h10 * 100.0,
        hyb_mrr,
    );
}
