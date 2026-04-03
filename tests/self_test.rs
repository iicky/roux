/// Self-test: index roux's own source code and verify retrieval quality.
/// Ignored by default — run with `cargo test -- --ignored test_self_retrieval`
#[test]
#[ignore]
fn test_self_retrieval() {
    use roux_cli::embed::Embedder;
    use roux_cli::embed::candle::CandleEmbedder;
    use roux_cli::extract;
    use roux_cli::model;
    use roux_cli::source::{Source, SourceKind};
    use roux_cli::store::Store;
    use roux_cli::store::sqlite::SqliteStore;

    // Index roux's own src/ directory
    let source = Source {
        name: "roux".to_string(),
        version: Some("dev".to_string()),
        kind: SourceKind::LocalPath(std::path::PathBuf::from("src")),
        language: Some("rust".to_string()),
    };

    let raw_chunks = extract::extract(&source).expect("extraction failed");
    assert!(
        raw_chunks.len() > 10,
        "expected >10 chunks from roux source, got {}",
        raw_chunks.len()
    );

    // Load model and embed
    let files = model::ensure_model(model::DEFAULT_MODEL_ID).expect("model download failed");
    let embedder =
        CandleEmbedder::load(&files.model, &files.tokenizer, &files.config).expect("load failed");

    let store = SqliteStore::open_in_memory_with_dim(embedder.embedding_dim()).unwrap();

    // Embed and store in batches
    for batch in raw_chunks.chunks(32) {
        let texts: Vec<&str> = batch.iter().map(|c| c.body.as_str()).collect();
        let embeddings = embedder.embed_passages(&texts).expect("embedding failed");

        let chunks: Vec<roux_cli::store::Chunk> = batch
            .iter()
            .zip(embeddings)
            .map(|(raw, embedding)| roux_cli::store::Chunk {
                id: raw.id(),
                source_name: raw.source_name.clone(),
                source_version: raw.source_version.clone(),
                language: raw.language.clone(),
                item_type: raw.item_type.clone(),
                qualified_name: raw.qualified_name.clone(),
                signature: raw.signature.clone(),
                doc: raw.doc.clone(),
                body: raw.body.clone(),
                embedding,
                url: raw.url.clone(),
                ingested_at: 0,
                score: None,
            })
            .collect();

        store.upsert_chunks(&chunks).unwrap();
    }

    // Known queries targeting public documented items (what the extractor actually indexes)
    let test_cases: Vec<(&str, &str)> = vec![
        (
            "resolve the database store path for local or global index",
            "resolve_store_path",
        ),
        (
            "open existing sqlite database and read embedding dimension",
            "open_existing",
        ),
        ("compute chunk ID from source and qualified name", "id"),
        ("find the right extractor for a source", "extract"),
        (
            "build embedding body text from item type and doc",
            "build_body",
        ),
        ("check model download status", "status"),
    ];

    let mut hits = 0;
    let total = test_cases.len();

    for (query, expected_substring) in &test_cases {
        let query_vec = embedder.embed_query(query).expect("query embed failed");
        let results = store
            .search(&query_vec, query, 5, None)
            .expect("search failed");

        let found = results
            .iter()
            .any(|r| r.qualified_name.contains(expected_substring));

        if found {
            hits += 1;
        } else {
            let top_names: Vec<&str> = results.iter().map(|r| r.qualified_name.as_str()).collect();
            eprintln!("MISS: query={query:?} expected={expected_substring:?} got={top_names:?}");
        }
    }

    let hit_rate = hits as f64 / total as f64;
    eprintln!("Self-test hit@5: {hits}/{total} ({:.0}%)", hit_rate * 100.0);

    // Require at least 50% hit rate — will tighten after model upgrade
    assert!(
        hit_rate >= 0.5,
        "hit@5 too low: {hits}/{total} ({:.0}%). Expected >= 50%",
        hit_rate * 100.0
    );
}
