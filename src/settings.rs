//! Runtime tuning knobs with sensible defaults, overridable via `ROUX_*`
//! environment variables.
//!
//! These are the internal levers of the retrieval pipeline — PPR parameters,
//! score-fusion exponents, ego-graph expansion caps, the extraction file-size
//! limit, the extraction thread stack. They are read once, at first access,
//! from the process environment (see [`get`]); an unset or unparseable variable
//! falls back to the default.
//!
//! | Variable                    | Default    | Meaning                                             |
//! |-----------------------------|------------|-----------------------------------------------------|
//! | `ROUX_PPR_ALPHA`            | `0.15`     | Personalized-PageRank restart probability           |
//! | `ROUX_PPR_ITERATIONS`      | `20`       | PPR power-iteration count                            |
//! | `ROUX_FUSION_BM25_EXP`     | `0.7`      | ScoreFusion BM25 exponent (α)                        |
//! | `ROUX_FUSION_PPR_EXP`      | `0.3`      | ScoreFusion PPR exponent (β)                         |
//! | `ROUX_RRF_K`               | `60.0`     | Reciprocal-rank-fusion constant                     |
//! | `ROUX_KIND_WEIGHT_FILE`    | `0.5`      | Score multiplier for `file` nodes                   |
//! | `ROUX_KIND_WEIGHT_DOC`     | `0.7`      | Score multiplier for `doc_section` nodes            |
//! | `ROUX_MAX_NODE_DEGREE`     | `128`      | Skip ego-expansion *through* nodes above this degree |
//! | `ROUX_MAX_SUBGRAPH_NODES`  | `4000`     | Hard cap on the ego-graph working set               |
//! | `ROUX_CANDIDATE_MULTIPLIER`| `2`        | BM25 over-fetch factor (`limit × N`)                 |
//! | `ROUX_MAX_FILE_BYTES`      | `10485760` | Max file size (bytes) considered during extraction  |
//! | `ROUX_WORKER_STACK_BYTES`  | `67108864` | Stack size for threads running the recursive AST walk |

use std::str::FromStr;
use std::sync::LazyLock;

/// Resolved tuning knobs for the retrieval pipeline.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// PPR restart probability (0.15 is standard).
    pub ppr_alpha: f64,
    /// PPR power-iteration count.
    pub ppr_iterations: usize,
    /// ScoreFusion BM25 exponent — `combined = bm25^α × ppr^β`.
    pub fusion_bm25_exp: f64,
    /// ScoreFusion PPR exponent.
    pub fusion_ppr_exp: f64,
    /// Floor applied to BM25 before ScoreFusion. Graph-expansion neighbors have
    /// no BM25 score (0), and the worst BM25 candidate min-max-normalizes to 0,
    /// so `bm25^α × ppr^β` would zero them out regardless of PPR — neighbors
    /// could never be promoted. Flooring BM25 at ε lets PPR rank the lexically
    /// weak/absent nodes.
    pub fusion_bm25_floor: f64,
    /// Reciprocal-rank-fusion constant `k` in `1/(k + rank)`.
    pub rrf_k: f64,
    /// Score multiplier demoting `file` nodes below code symbols.
    pub kind_weight_file: f64,
    /// Score multiplier demoting `doc_section` nodes below code symbols.
    pub kind_weight_doc: f64,
    /// Ego-graph expansion skips *through* any node with more neighbors than
    /// this — hubs stay in the graph, but their neighbors don't flood the set.
    pub max_node_degree: usize,
    /// Hard backstop on the ego-graph working-set size, bounding PPR cost.
    pub max_subgraph_nodes: usize,
    /// BM25 over-fetch factor: a search pulls `limit × this` candidates so
    /// graph re-ranking has room to promote neighbors over raw lexical hits.
    pub candidate_multiplier: usize,
    /// Files larger than this (in bytes) are skipped during extraction.
    pub max_file_bytes: usize,
    /// Stack size (bytes) for threads that run the recursive-descent AST walk.
    /// Deeply nested syntax recurses as deep as it nests, so extraction needs a
    /// far larger stack than the ~2 MB thread default.
    pub worker_stack_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ppr_alpha: 0.15,
            ppr_iterations: 20,
            fusion_bm25_exp: 0.7,
            fusion_ppr_exp: 0.3,
            fusion_bm25_floor: 0.05,
            rrf_k: 60.0,
            kind_weight_file: 0.5,
            kind_weight_doc: 0.7,
            max_node_degree: 128,
            max_subgraph_nodes: 4000,
            candidate_multiplier: 2,
            max_file_bytes: 10 * 1024 * 1024,
            worker_stack_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Settings {
    /// Resolve from the process environment, falling back to defaults.
    fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Resolve each field from a lookup function. Split out from [`from_env`] so
    /// override behavior is testable without mutating the process environment.
    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let d = Self::default();
        Self {
            ppr_alpha: parse_or(&get, "ROUX_PPR_ALPHA", d.ppr_alpha),
            ppr_iterations: parse_or(&get, "ROUX_PPR_ITERATIONS", d.ppr_iterations),
            fusion_bm25_exp: parse_or(&get, "ROUX_FUSION_BM25_EXP", d.fusion_bm25_exp),
            fusion_ppr_exp: parse_or(&get, "ROUX_FUSION_PPR_EXP", d.fusion_ppr_exp),
            fusion_bm25_floor: parse_or(&get, "ROUX_FUSION_BM25_FLOOR", d.fusion_bm25_floor),
            rrf_k: parse_or(&get, "ROUX_RRF_K", d.rrf_k),
            kind_weight_file: parse_or(&get, "ROUX_KIND_WEIGHT_FILE", d.kind_weight_file),
            kind_weight_doc: parse_or(&get, "ROUX_KIND_WEIGHT_DOC", d.kind_weight_doc),
            max_node_degree: parse_or(&get, "ROUX_MAX_NODE_DEGREE", d.max_node_degree),
            max_subgraph_nodes: parse_or(&get, "ROUX_MAX_SUBGRAPH_NODES", d.max_subgraph_nodes),
            candidate_multiplier: parse_or(
                &get,
                "ROUX_CANDIDATE_MULTIPLIER",
                d.candidate_multiplier,
            )
            .max(1),
            max_file_bytes: parse_or(&get, "ROUX_MAX_FILE_BYTES", d.max_file_bytes),
            worker_stack_bytes: parse_or(&get, "ROUX_WORKER_STACK_BYTES", d.worker_stack_bytes)
                .max(1024 * 1024),
        }
    }
}

/// Parse `key` from the lookup, warning (and keeping the default) on a value
/// that is present but unparseable so typos don't silently change behavior.
fn parse_or<T: FromStr>(get: &impl Fn(&str) -> Option<String>, key: &str, default: T) -> T {
    match get(key) {
        None => default,
        Some(raw) => match raw.parse() {
            Ok(v) => v,
            Err(_) => {
                eprintln!("roux: ignoring invalid {key}={raw:?}, using default");
                default
            }
        },
    }
}

static SETTINGS: LazyLock<Settings> = LazyLock::new(Settings::from_env);

/// The process-wide tuning knobs, resolved once from the environment.
pub fn get() -> &'static Settings {
    &SETTINGS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_historical_constants() {
        let d = Settings::default();
        assert_eq!(d.ppr_alpha, 0.15);
        assert_eq!(d.ppr_iterations, 20);
        assert_eq!(d.fusion_bm25_exp, 0.7);
        assert_eq!(d.fusion_ppr_exp, 0.3);
        assert_eq!(d.fusion_bm25_floor, 0.05);
        assert_eq!(d.rrf_k, 60.0);
        assert_eq!(d.max_node_degree, 128);
        assert_eq!(d.max_subgraph_nodes, 4000);
        assert_eq!(d.candidate_multiplier, 2);
        assert_eq!(d.max_file_bytes, 10 * 1024 * 1024);
        assert_eq!(d.worker_stack_bytes, 64 * 1024 * 1024);
    }

    #[test]
    fn empty_env_yields_defaults() {
        let s = Settings::from_lookup(|_| None);
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn env_overrides_are_applied() {
        let s = Settings::from_lookup(|k| match k {
            "ROUX_MAX_NODE_DEGREE" => Some("64".to_string()),
            "ROUX_PPR_ALPHA" => Some("0.25".to_string()),
            "ROUX_MAX_FILE_BYTES" => Some("1048576".to_string()),
            _ => None,
        });
        assert_eq!(s.max_node_degree, 64);
        assert_eq!(s.ppr_alpha, 0.25);
        assert_eq!(s.max_file_bytes, 1048576);
        // Untouched knobs keep their defaults.
        assert_eq!(s.max_subgraph_nodes, 4000);
    }

    #[test]
    fn invalid_value_falls_back_to_default() {
        let s = Settings::from_lookup(|k| {
            (k == "ROUX_MAX_NODE_DEGREE").then(|| "not-a-number".to_string())
        });
        assert_eq!(s.max_node_degree, 128);
    }

    #[test]
    fn candidate_multiplier_is_floored_at_one() {
        // A 0 multiplier would starve re-ranking of candidates; clamp it up.
        let s =
            Settings::from_lookup(|k| (k == "ROUX_CANDIDATE_MULTIPLIER").then(|| "0".to_string()));
        assert_eq!(s.candidate_multiplier, 1);
    }
}
