use std::collections::{HashMap, HashSet};

use petgraph::graph::{DiGraph, NodeIndex};

use super::{Edge, Node};

/// A ranked subgraph result from PPR-scored ego-graph expansion.
pub struct RankedSubgraph {
    /// All nodes in the subgraph, ordered by PPR score (highest first)
    pub nodes: Vec<ScoredNode>,
    /// All edges between nodes in the subgraph
    pub edges: Vec<Edge>,
}

pub struct ScoredNode {
    pub node: Node,
    pub score: f64,
    /// Whether this node was a direct BM25 match
    pub is_seed: bool,
}

/// Build a petgraph from nodes + edges, run PPR from seed nodes,
/// return the top-k scored subgraph.
pub fn rank_subgraph(
    nodes: Vec<Node>,
    edges: Vec<Edge>,
    seed_ids: &[String],
    top_k: usize,
) -> RankedSubgraph {
    if nodes.is_empty() {
        return RankedSubgraph {
            nodes: vec![],
            edges: vec![],
        };
    }

    // Build petgraph
    let mut graph = DiGraph::<String, String>::new();
    let mut id_to_idx: HashMap<String, NodeIndex> = HashMap::new();
    let mut idx_to_id: HashMap<NodeIndex, String> = HashMap::new();

    for node in &nodes {
        let idx = graph.add_node(node.id.clone());
        id_to_idx.insert(node.id.clone(), idx);
        idx_to_id.insert(idx, node.id.clone());
    }

    for edge in &edges {
        if let (Some(&from), Some(&to)) = (id_to_idx.get(&edge.from_id), id_to_idx.get(&edge.to_id))
        {
            graph.add_edge(from, to, edge.kind.clone());
            // Add reverse edge for undirected traversal (both directions matter)
            graph.add_edge(to, from, format!("rev_{}", edge.kind));
        }
    }

    // Add parent_id as implicit "contains" edges so PPR flows through containment
    for node in &nodes {
        if let Some(ref parent_id) = node.parent_id
            && let (Some(&child), Some(&parent)) =
                (id_to_idx.get(&node.id), id_to_idx.get(parent_id))
        {
            graph.add_edge(parent, child, "contains".to_string());
            graph.add_edge(child, parent, "contained_by".to_string());
        }
    }

    // Run Personalized PageRank
    let seed_indices: Vec<NodeIndex> = seed_ids
        .iter()
        .filter_map(|id| id_to_idx.get(id).copied())
        .collect();

    let scores = personalized_pagerank(&graph, &seed_indices, 0.15, 20);

    // Rank nodes by score
    let mut scored: Vec<(String, f64)> = scores
        .iter()
        .map(|(idx, score)| (idx_to_id[idx].clone(), *score))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Take top-k
    let seed_set: HashSet<&str> = seed_ids.iter().map(|s| s.as_str()).collect();
    let top_ids: HashSet<String> = scored
        .iter()
        .take(top_k)
        .map(|(id, _)| id.clone())
        .collect();

    // Build result — seed nodes always included
    let mut result_ids: HashSet<String> = top_ids;
    for seed in seed_ids {
        result_ids.insert(seed.clone());
    }

    let node_map: HashMap<&str, &Node> = nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut result_nodes: Vec<ScoredNode> = scored
        .iter()
        .filter(|(id, _)| result_ids.contains(id))
        .filter_map(|(id, score)| {
            node_map.get(id.as_str()).map(|n| ScoredNode {
                node: (*n).clone(),
                score: *score,
                is_seed: seed_set.contains(id.as_str()),
            })
        })
        .collect();

    // Sort: seeds first, then by score
    result_nodes.sort_by(|a, b| {
        b.is_seed.cmp(&a.is_seed).then(
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });

    // Filter edges to only those between result nodes
    let result_edges: Vec<Edge> = edges
        .into_iter()
        .filter(|e| result_ids.contains(&e.from_id) && result_ids.contains(&e.to_id))
        .collect();

    RankedSubgraph {
        nodes: result_nodes,
        edges: result_edges,
    }
}

/// Personalized PageRank via power iteration.
///
/// `alpha` is the restart probability (0.15 is standard).
/// Returns a map of node index → PPR score.
fn personalized_pagerank(
    graph: &DiGraph<String, String>,
    seeds: &[NodeIndex],
    alpha: f64,
    iterations: usize,
) -> HashMap<NodeIndex, f64> {
    let n = graph.node_count();
    if n == 0 || seeds.is_empty() {
        return HashMap::new();
    }

    // Personalization vector: uniform over seeds
    let seed_weight = 1.0 / seeds.len() as f64;
    let mut personalization: HashMap<NodeIndex, f64> = HashMap::new();
    for &seed in seeds {
        personalization.insert(seed, seed_weight);
    }

    // Initialize scores
    let mut scores: HashMap<NodeIndex, f64> = personalization.clone();

    for _ in 0..iterations {
        let mut new_scores: HashMap<NodeIndex, f64> = HashMap::new();

        // Distribute each node's score to its neighbors
        for idx in graph.node_indices() {
            let score = scores.get(&idx).copied().unwrap_or(0.0);
            let neighbors: Vec<NodeIndex> = graph.neighbors(idx).collect();
            let out_degree = neighbors.len();

            if out_degree > 0 {
                let share = score / out_degree as f64;
                for neighbor in neighbors {
                    *new_scores.entry(neighbor).or_default() += (1.0 - alpha) * share;
                }
            }
        }

        // Add restart probability
        for (&seed, &weight) in &personalization {
            *new_scores.entry(seed).or_default() += alpha * weight;
        }

        scores = new_scores;
    }

    // Normalize
    let total: f64 = scores.values().sum();
    if total > 0.0 {
        for score in scores.values_mut() {
            *score /= total;
        }
    }

    scores
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_node(id: &str, name: &str) -> Node {
        Node {
            id: id.to_string(),
            kind: "function".to_string(),
            name: name.to_string(),
            qualified_name: format!("test::{name}"),
            source_name: "test".to_string(),
            language: "rust".to_string(),
            file_path: "lib.rs".to_string(),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            visibility: "pub".to_string(),
            signature: Some(format!("fn {name}()")),
            doc: None,
            body: format!("function: {name}"),
            parent_id: None,
            content_hash: None,
            line_count: 10,
            source_url: None,
        }
    }

    #[test]
    fn test_ppr_ranks_connected_nodes_higher() {
        let nodes = vec![
            make_test_node("a", "authenticate"),
            make_test_node("b", "validate_token"),
            make_test_node("c", "hash_password"),
            make_test_node("d", "unrelated_function"),
        ];

        let edges = vec![
            Edge {
                from_id: "a".to_string(),
                to_id: "b".to_string(),
                kind: "calls".to_string(),
            },
            Edge {
                from_id: "a".to_string(),
                to_id: "c".to_string(),
                kind: "calls".to_string(),
            },
        ];

        let result = rank_subgraph(nodes, edges, &["a".to_string()], 10);

        // authenticate should be highest (it's the seed)
        assert_eq!(result.nodes[0].node.name, "authenticate");
        assert!(result.nodes[0].is_seed);

        // validate_token and hash_password should be in results (connected to seed)
        let names: Vec<&str> = result.nodes.iter().map(|n| n.node.name.as_str()).collect();
        assert!(
            names.contains(&"validate_token"),
            "connected node should be in results"
        );
        assert!(
            names.contains(&"hash_password"),
            "connected node should be in results"
        );

        // unrelated_function may not be in results at all (no edge connection)
        // If it is, it should rank lower
        if let Some(unrelated_pos) = names.iter().position(|n| *n == "unrelated_function") {
            let validate_pos = names.iter().position(|n| *n == "validate_token").unwrap();
            assert!(
                validate_pos < unrelated_pos,
                "connected node should rank higher than unrelated"
            );
        }
    }

    #[test]
    fn test_ppr_hub_node_ranks_high() {
        // Hub node connects to many things — should rank high
        let nodes = vec![
            make_test_node("seed", "query"),
            make_test_node("hub", "database"),
            make_test_node("a", "connect"),
            make_test_node("b", "execute"),
            make_test_node("c", "transaction"),
        ];

        let edges = vec![
            Edge {
                from_id: "seed".to_string(),
                to_id: "hub".to_string(),
                kind: "calls".to_string(),
            },
            Edge {
                from_id: "hub".to_string(),
                to_id: "a".to_string(),
                kind: "calls".to_string(),
            },
            Edge {
                from_id: "hub".to_string(),
                to_id: "b".to_string(),
                kind: "calls".to_string(),
            },
            Edge {
                from_id: "hub".to_string(),
                to_id: "c".to_string(),
                kind: "calls".to_string(),
            },
        ];

        let result = rank_subgraph(nodes, edges, &["seed".to_string()], 10);

        // Hub should rank second (after seed)
        let names: Vec<&str> = result.nodes.iter().map(|n| n.node.name.as_str()).collect();
        assert_eq!(names[0], "query"); // seed
        assert_eq!(names[1], "database"); // hub
    }

    #[test]
    fn test_top_k_limits_output() {
        let nodes: Vec<Node> = (0..20)
            .map(|i| make_test_node(&format!("n{i}"), &format!("func_{i}")))
            .collect();

        let edges = vec![Edge {
            from_id: "n0".to_string(),
            to_id: "n1".to_string(),
            kind: "calls".to_string(),
        }];

        let result = rank_subgraph(nodes, edges, &["n0".to_string()], 5);

        // Should have at most ~5 nodes (seeds always included)
        assert!(result.nodes.len() <= 6, "got {}", result.nodes.len());
    }

    #[test]
    fn test_empty_graph() {
        let result = rank_subgraph(vec![], vec![], &[], 5);
        assert!(result.nodes.is_empty());
    }
}
