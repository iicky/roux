//! Shared strict match predicate for the bench harnesses.
//!
//! A gold "hits" a ranked symbol when the gold's identifier *tokens* appear as a
//! contiguous run inside the symbol name's tokens — word-boundary matching, not
//! any-substring. Tokenizing splits snake_case, kebab/dot separators, and
//! camelCase (including acronym boundaries like `JSONParser` -> [json, parser]).
//! So `search` no longer matches `research`, but `JSON` still matches `BindJSON`
//! and `Searcher` still matches `SearcherBuilder`.
//!
//! Mirrors `bench/bench_match.py`; keep the two in sync (unification tracked in
//! roux-mj3k).

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
}
