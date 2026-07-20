//! Shared strict match predicate for the bench harnesses.
//!
//! A gold "hits" a ranked symbol when the gold's identifier *tokens* appear as a
//! contiguous run inside the symbol name's tokens — word-boundary matching, not
//! any-substring. Tokenizing splits snake_case, kebab/dot separators, and
//! camelCase (including acronym boundaries like `JSONParser` -> [json, parser]).
//! So `search` no longer matches `research`, but `JSON` still matches `BindJSON`
//! and `Searcher` still matches `SearcherBuilder`.
//!
//! Mirrors `bench/bench_match.py`; keep the two in sync (they intentionally duplicate matching logic).

#![allow(dead_code)]

/// Split an identifier into lowercased word tokens.
pub fn tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for i in 0..chars.len() {
        let c = chars[i];
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if !cur.is_empty() {
            let prev = chars[i - 1];
            // lower/digit -> Upper: camelCase boundary (bindJSON -> bind|JSON)
            let camel = c.is_uppercase() && (prev.is_lowercase() || prev.is_numeric());
            // Upper -> Upper-then-lower: acronym end (JSONParser -> JSON|Parser)
            let acronym = c.is_uppercase()
                && prev.is_uppercase()
                && i + 1 < chars.len()
                && chars[i + 1].is_lowercase();
            if camel || acronym {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True when `gold`'s token sequence appears contiguously within `name`'s tokens.
pub fn strict_match(name: &str, gold: &str) -> bool {
    let g = tokens(gold);
    if g.is_empty() {
        return false;
    }
    let n = tokens(name);
    if g.len() > n.len() {
        return false;
    }
    n.windows(g.len()).any(|w| w == g.as_slice())
}

/// True when any of `expected` strictly matches `name`.
pub fn any_match(name: &str, expected: &[&str]) -> bool {
    expected.iter().any(|exp| strict_match(name, exp))
}

/// Hit@K: fraction of queries with at least one expected symbol in the top K.
pub fn hit_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let hits = results
        .iter()
        .filter(|(names, expected)| names.iter().take(k).any(|n| any_match(n, expected)))
        .count();
    hits as f64 / results.len() as f64
}

/// MRR: mean reciprocal rank of the first matching result per query.
pub fn mrr(results: &[(Vec<String>, &[&str])]) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let sum: f64 = results
        .iter()
        .map(|(names, expected)| {
            names
                .iter()
                .position(|n| any_match(n, expected))
                .map(|i| 1.0 / (i + 1) as f64)
                .unwrap_or(0.0)
        })
        .sum();
    sum / results.len() as f64
}

/// NDCG@K. Each expected symbol is credited at most once — the first top-K
/// result that strictly matches an as-yet-uncredited gold — so duplicate
/// substring matches can't push DCG past the ideal DCG and NDCG stays in [0, 1].
pub fn ndcg_at_k(results: &[(Vec<String>, &[&str])], k: usize) -> f64 {
    if results.is_empty() {
        return 0.0;
    }
    let sum: f64 = results
        .iter()
        .map(|(names, expected)| {
            let mut credited = vec![false; expected.len()];
            let mut dcg = 0.0f64;
            for (i, name) in names.iter().take(k).enumerate() {
                if let Some(gi) = (0..expected.len())
                    .find(|&gi| !credited[gi] && strict_match(name, expected[gi]))
                {
                    credited[gi] = true;
                    dcg += 1.0 / (i as f64 + 2.0).log2();
                }
            }
            let n_relevant = expected.len().min(k);
            let idcg: f64 = (0..n_relevant).map(|i| 1.0 / (i as f64 + 2.0).log2()).sum();
            if idcg > 0.0 { dcg / idcg } else { 0.0 }
        })
        .sum();
    sum / results.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_boundary_semantics() {
        assert!(strict_match("BindJSON", "JSON"));
        assert!(strict_match("SearcherBuilder", "Searcher"));
        assert!(strict_match("merge_ordered", "merge"));
        assert!(strict_match("M140", "M140"));
        assert!(strict_match("createCookieSessionStorage", "SessionStorage"));
        // No longer over-matches on substrings:
        assert!(!strict_match("research", "search"));
        assert!(!strict_match("merge_ordered", "merge_asof"));
        assert!(!strict_match("Searcher", "build"));
    }

    #[test]
    fn ndcg_never_exceeds_one_on_duplicate_matches() {
        // Regression: a single gold matched by *multiple* top-k results must not
        // let NDCG exceed 1.0 (each gold can only be credited once).
        let results: Vec<(Vec<String>, &[&str])> = vec![(
            vec![
                "handler".to_string(),
                "request_handler".to_string(),
                "handler".to_string(),
            ],
            &["handler"][..],
        )];
        let ndcg = ndcg_at_k(&results, 10);
        assert!(ndcg <= 1.0, "ndcg {ndcg} exceeded 1.0 on duplicate matches");
        assert!(
            ndcg > 0.0,
            "ndcg {ndcg} should be positive: gold was matched"
        );
    }

    #[test]
    fn ndcg_is_one_when_golds_matched_at_top() {
        // Single gold, first result matches -> perfect NDCG.
        let single: Vec<(Vec<String>, &[&str])> = vec![(
            vec!["parse_input".to_string(), "other".to_string()],
            &["parse"][..],
        )];
        assert!((ndcg_at_k(&single, 5) - 1.0).abs() < 1e-9);

        // Multiple distinct golds, all matched by the top results in gold order.
        let multi: Vec<(Vec<String>, &[&str])> = vec![(
            vec![
                "alpha_fn".to_string(),
                "beta_fn".to_string(),
                "gamma_fn".to_string(),
            ],
            &["alpha", "beta", "gamma"][..],
        )];
        assert!((ndcg_at_k(&multi, 3) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn ndcg_mean_stays_in_unit_interval_for_mixed_queries() {
        let results: Vec<(Vec<String>, &[&str])> = vec![
            // Hit buried at rank 3.
            (
                vec![
                    "irrelevant1".to_string(),
                    "irrelevant2".to_string(),
                    "target_fn".to_string(),
                ],
                &["target"][..],
            ),
            // Complete miss.
            (
                vec!["a".to_string(), "b".to_string(), "c".to_string()],
                &["missing"][..],
            ),
            // Two golds, one matched immediately, one matched deeper.
            (
                vec![
                    "foo_impl".to_string(),
                    "other".to_string(),
                    "bar_impl".to_string(),
                ],
                &["foo", "bar"][..],
            ),
        ];
        let mean = ndcg_at_k(&results, 10);
        assert!((0.0..=1.0).contains(&mean), "ndcg mean {mean} out of [0,1]");
        // Mixed hits and a total miss: neither perfect nor zero.
        assert!(mean > 0.0 && mean < 1.0);
    }

    #[test]
    fn metrics_on_empty_results_are_zero() {
        let empty: Vec<(Vec<String>, &[&str])> = vec![];
        assert_eq!(ndcg_at_k(&empty, 10), 0.0);
        assert_eq!(hit_at_k(&empty, 1), 0.0);
        assert_eq!(mrr(&empty), 0.0);
    }

    #[test]
    fn hit_at_k_and_mrr_exact_fractions() {
        let results: Vec<(Vec<String>, &[&str])> = vec![
            // Match at rank 3 (index 2).
            (
                vec!["a".to_string(), "b".to_string(), "target_fn".to_string()],
                &["target"][..],
            ),
            // Match at rank 1 (index 0).
            (
                vec!["foo_impl".to_string(), "x".to_string(), "y".to_string()],
                &["foo"][..],
            ),
            // No match at all.
            (
                vec!["a".to_string(), "b".to_string(), "c".to_string()],
                &["zzz"][..],
            ),
        ];

        // k=2 misses the rank-3 hit, still counts the rank-1 hit: 1/3.
        assert_eq!(hit_at_k(&results, 2), 1.0 / 3.0);
        // k=3 counts both hits: 2/3.
        assert_eq!(hit_at_k(&results, 3), 2.0 / 3.0);

        // Reciprocal ranks: 1/3, 1/1, 0 -> mean = (1/3 + 1 + 0) / 3 = 4/9.
        let expected_mrr = (1.0 / 3.0 + 1.0 + 0.0) / 3.0;
        assert!((mrr(&results) - expected_mrr).abs() < 1e-9);
    }
}
