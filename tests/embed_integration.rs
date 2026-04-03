/// Integration test that downloads the model and runs real inference.
/// Ignored by default — run with `cargo test -- --ignored` or explicitly.
#[test]
#[ignore]
fn test_embed_e5_small() {
    use roux_cli::embed::Embedder;
    use roux_cli::embed::candle::CandleEmbedder;
    use roux_cli::model;

    // Download/ensure model files
    let files = model::ensure_model(model::DEFAULT_MODEL_ID).expect("model download failed");

    // Load embedder
    let embedder =
        CandleEmbedder::load(&files.model, &files.tokenizer, &files.config).expect("load failed");

    // Embed a query
    let query_vec = embedder
        .embed_query("how to spawn a task")
        .expect("query embed failed");
    assert_eq!(query_vec.len(), embedder.embedding_dim());

    // Check it's normalized (L2 norm ≈ 1.0)
    let norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 0.01, "expected unit norm, got {norm}");

    // Embed passages
    let passages = &["Spawns a new async task", "Reads a file from disk"];
    let passage_vecs = embedder
        .embed_passages(passages)
        .expect("passage embed failed");
    assert_eq!(passage_vecs.len(), 2);
    assert_eq!(passage_vecs[0].len(), embedder.embedding_dim());

    // Query should be more similar to the first passage
    let sim0: f32 = query_vec
        .iter()
        .zip(&passage_vecs[0])
        .map(|(a, b)| a * b)
        .sum();
    let sim1: f32 = query_vec
        .iter()
        .zip(&passage_vecs[1])
        .map(|(a, b)| a * b)
        .sum();
    assert!(
        sim0 > sim1,
        "expected 'spawn a task' to be closer to 'Spawns a new async task' than 'Reads a file from disk', got {sim0} vs {sim1}"
    );
}
