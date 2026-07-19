//! Agent-legibility audit: turn retrieval misses into refactor recommendations.
//!
//! For each public symbol, measure how findable it is to roux's own retrieval
//! and classify the cheapest fix when it is hard to surface. Four failure
//! modes, each with a distinct remedy:
//!
//! - [`Cause::Unnameable`] — the name has no queryable tokens (1–2 characters),
//!   so it can't be found by name at all. The fix is a longer, descriptive name.
//! - [`Cause::Collision`] — the symbol is buried even for a query built from its
//!   own name, because same-named siblings out-compete it. Adding docs does not
//!   help; the fix is a rename or a distinctive term.
//! - [`Cause::VocabGap`] — the symbol has a distinctive name but misses an
//!   *intent* query phrased without that name, because no doc bridges concept to
//!   code. The fix is one honest doc line naming what it does.
//! - [`Cause::Isolation`] — the symbol has no callers/callees/children in the
//!   graph, so ranking can only reach it by exact lexical match. The fix is to
//!   wire it into the call graph (or, failing that, document it).
//!
//! This is CPU-only and reuses [`extract`](crate::graph::extract) plus
//! [`GraphStore::search`](crate::graph::store::GraphStore::search); it adds no
//! dependencies. The deterministic taxonomy lives in [`classify`]; signal
//! gathering (which runs retrieval probes) lives in [`audit`].

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::graph::extract::FileGraph;
use crate::graph::store::GraphStore;

/// How many results a probe query fetches; a symbol outside this window "misses".
const PROBE_LIMIT: usize = 10;
/// A symbol ranked beyond this (1-based) for its own name is "buried".
const BURIED_RANK: usize = 5;

/// Identifier heads too generic to carry intent on their own.
const GENERIC_NAMES: &[&str] = &[
    "run", "handle", "process", "get", "set", "new", "build", "make", "exec", "execute", "call",
    "apply", "start", "stop", "init", "update", "check", "load", "save", "read", "write", "open",
    "close", "parse", "convert", "do",
];

/// Why a public symbol is hard for an agent to surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// Findable and well-connected — no action needed.
    Ok,
    /// Name has no queryable tokens (1–2 chars) → give it a descriptive name.
    Unnameable,
    /// Buried even for its own name by same-named siblings → rename/disambiguate.
    Collision,
    /// Distinctive name but no concept→code bridge → add a doc line.
    VocabGap,
    /// No graph edges → wire it into the call graph.
    Isolation,
}

impl Cause {
    /// Short machine label for JSON output.
    pub fn label(self) -> &'static str {
        match self {
            Cause::Ok => "ok",
            Cause::Unnameable => "unnameable",
            Cause::Collision => "collision",
            Cause::VocabGap => "vocab_gap",
            Cause::Isolation => "isolation",
        }
    }

    /// The recommended refactor for this cause.
    pub fn fix(self) -> &'static str {
        match self {
            Cause::Ok => "",
            Cause::Unnameable => {
                "rename to a longer, descriptive identifier — a 1-2 character name has no tokens to search for"
            }
            Cause::Collision => {
                "rename or add a distinctive term — same-named siblings out-compete it (docs alone won't fix)"
            }
            Cause::VocabGap => {
                "add a one-line doc naming what it does in intent terms (the proven doc-bridge)"
            }
            Cause::Isolation => "wire it into the call graph, or add a bridging doc line",
        }
    }
}

/// The raw retrieval + graph signals for one symbol, fed to [`classify`].
#[derive(Debug, Clone)]
pub struct Signals {
    pub has_doc: bool,
    pub isolated: bool,
    pub generic_name: bool,
    /// Whether the name yields any queryable token (false for 1–2 char names).
    pub nameable: bool,
    /// Rank (1-based) of the symbol for a query built from its own name; `None`
    /// means it fell outside the probe window entirely.
    pub self_name_rank: Option<usize>,
    /// Whether an intent query (name tokens stripped) was actually run.
    pub intent_probed: bool,
    /// Rank (1-based) for the best intent query; `None` means it missed.
    pub intent_rank: Option<usize>,
}

/// The legibility verdict for one public symbol.
#[derive(Debug, Clone)]
pub struct SymbolLegibility {
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: usize,
    pub signals: Signals,
    pub cause: Cause,
    pub severity: u8,
    pub reasons: Vec<String>,
}

impl SymbolLegibility {
    /// True when this symbol is genuinely hard to surface and worth reporting.
    pub fn is_finding(&self) -> bool {
        self.cause != Cause::Ok
    }
}

/// Classify a symbol from its signals into a dominant [`Cause`], a severity, and
/// human-readable reasons. Pure and deterministic — the corpus-sensitive part
/// (running the probes) lives in [`audit`].
///
/// Priority is worst-first: `Unnameable` (no queryable tokens) and `Collision`
/// (buried even for its own name) both mean "unfindable by name" and outrank a
/// `VocabGap` (findable by name, not by concept), which outranks `Isolation`.
/// A collision is never "fixed" by docs alone.
pub fn classify(s: &Signals) -> (Cause, u8, Vec<String>) {
    let intent_findable = s.intent_probed && s.intent_rank.is_some_and(|r| r <= BURIED_RANK);
    // A name with no queryable tokens (1–2 chars) can't be found by name at all;
    // only a real (human) doc that makes it concept-findable can rescue it. An
    // auto-generated description doesn't count — it tends to echo the name.
    let unnameable = !(s.nameable || (s.has_doc && intent_findable));
    // Has tokens, but buried even for them: same-named siblings out-compete it.
    let collided = s.nameable && s.self_name_rank.is_none_or(|r| r > BURIED_RANK);
    // Distinctive & findable by name, but an intent query that avoids the name misses.
    let vocab_miss =
        s.nameable && !collided && s.intent_probed && s.intent_rank.is_none_or(|r| r > BURIED_RANK);

    let mut reasons = Vec::new();
    let mut severity: u8 = 0;
    if unnameable {
        severity += 2;
        reasons.push("name has no queryable tokens (too short to search for)".into());
    }
    if collided {
        match s.self_name_rank {
            None => {
                severity += 3;
                reasons.push("misses the top 10 for a query built from its own name".into());
            }
            Some(r) => {
                severity += 2;
                reasons.push(format!("ranks #{r} for its own name (name collision)"));
            }
        }
    }
    if vocab_miss {
        severity += 2;
        match s.intent_rank {
            None => reasons
                .push("misses an intent query that avoids its name (no concept bridge)".into()),
            Some(r) => reasons.push(format!(
                "ranks #{r} for an intent query that avoids its name"
            )),
        }
    }
    if !s.has_doc {
        severity += 1;
        reasons.push("undocumented".into());
    }
    if s.isolated {
        severity += 1;
        reasons.push("graph-isolated (no callers/callees/children)".into());
    }
    if s.generic_name {
        severity += 1;
        reasons.push("generic name".into());
    }

    // Dominant cause, worst-first. A symbol is only a finding when it is
    // genuinely hard to surface — undocumented-but-findable is fine.
    let cause = if unnameable {
        Cause::Unnameable
    } else if collided {
        Cause::Collision
    } else if vocab_miss {
        Cause::VocabGap
    } else if s.isolated && !s.has_doc {
        Cause::Isolation
    } else {
        Cause::Ok
    };

    (cause, severity, reasons)
}

/// Split free text (a doc sentence or identifier) into lowercase word tokens.
fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(|w| w.to_lowercase())
        .collect()
}

/// Derive an intent query for a symbol: the first sentence of its doc (or, failing
/// that, its auto-generated description) with the symbol's own name tokens
/// removed, so a hit reflects concept findability rather than a token echo.
/// Returns `None` when there is no description to phrase intent from.
fn derive_intent(name: &str, doc: Option<&str>, description: Option<&str>) -> Option<String> {
    let text = doc.or(description)?;
    let first = text.split(['.', '\n']).next().unwrap_or(text);
    let own: HashSet<String> = words(name).into_iter().collect();
    let intent: Vec<String> = words(first)
        .into_iter()
        .filter(|w| !own.contains(w))
        .collect();
    if intent.len() < 2 {
        None
    } else {
        Some(intent.join(" "))
    }
}

/// 1-based rank of `id` in a search for `query`, or `None` if outside the window.
fn rank_of(store: &GraphStore, query: &str, id: &str) -> Result<Option<usize>> {
    let res = store.search(query, PROBE_LIMIT)?;
    Ok(res.nodes.iter().position(|n| n.id == id).map(|r| r + 1))
}

/// Audit every public symbol in `graph`, using `store` (already populated with
/// the same graph) for retrieval probes. `authored` optionally maps a symbol's
/// qualified name to hand-written intent queries that override the derived one.
/// Results are sorted findings-first, worst severity first.
pub fn audit(
    graph: &FileGraph,
    store: &GraphStore,
    authored: Option<&HashMap<String, Vec<String>>>,
) -> Result<Vec<SymbolLegibility>> {
    // Cross-reference degree per node, and the set of nodes that are a parent.
    let mut degree: HashMap<&str, usize> = HashMap::new();
    for e in &graph.edges {
        *degree.entry(e.from_id.as_str()).or_default() += 1;
        *degree.entry(e.to_id.as_str()).or_default() += 1;
    }
    let has_children: HashSet<&str> = graph
        .nodes
        .iter()
        .filter_map(|n| n.parent_id.as_deref())
        .collect();

    let mut out = Vec::new();
    for n in &graph.nodes {
        if matches!(n.kind.as_str(), "file" | "module" | "doc_section") {
            continue;
        }
        // The public API surface an agent queries for: everything not explicitly
        // private. This covers `pub`/`export` and the no-visibility-concept
        // languages (C/C++/Bash), whose symbols carry an empty visibility.
        if n.visibility == "private" {
            continue;
        }

        let deg = degree.get(n.id.as_str()).copied().unwrap_or(0);

        // Collision probe: query built from the symbol's own name.
        let self_query = words(&n.name).join(" ");
        let self_name_rank = if self_query.is_empty() {
            None
        } else {
            rank_of(store, &self_query, &n.id)?
        };

        // Vocab probe: authored intent queries, else one derived from the doc.
        let intent_queries: Vec<String> = authored
            .and_then(|m| m.get(&n.qualified_name).cloned())
            .or_else(|| {
                derive_intent(&n.name, n.doc.as_deref(), n.description.as_deref()).map(|q| vec![q])
            })
            .unwrap_or_default();
        let intent_probed = !intent_queries.is_empty();
        let mut intent_rank: Option<usize> = None;
        for q in &intent_queries {
            if let Some(r) = rank_of(store, q, &n.id)? {
                intent_rank = Some(intent_rank.map_or(r, |cur| cur.min(r)));
            }
        }

        let signals = Signals {
            has_doc: n.doc.is_some(),
            isolated: deg == 0 && !has_children.contains(n.id.as_str()),
            generic_name: GENERIC_NAMES.contains(&n.name.to_lowercase().as_str()),
            self_name_rank,
            intent_probed,
            intent_rank,
            nameable: !self_query.is_empty(),
        };
        let (cause, severity, reasons) = classify(&signals);

        out.push(SymbolLegibility {
            qualified_name: n.qualified_name.clone(),
            file_path: n.file_path.clone(),
            start_line: n.start_line,
            signals,
            cause,
            severity,
            reasons,
        });
    }

    out.sort_by(|a, b| {
        b.is_finding()
            .cmp(&a.is_finding())
            .then(b.severity.cmp(&a.severity))
            .then(a.file_path.cmp(&b.file_path))
            .then(a.start_line.cmp(&b.start_line))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::extract::extract_dir;

    fn sig(
        has_doc: bool,
        isolated: bool,
        generic_name: bool,
        self_name_rank: Option<usize>,
        intent_probed: bool,
        intent_rank: Option<usize>,
    ) -> Signals {
        Signals {
            has_doc,
            isolated,
            generic_name,
            self_name_rank,
            intent_probed,
            intent_rank,
            nameable: true,
        }
    }

    // --- classifier taxonomy (pure, corpus-independent) ---

    #[test]
    fn buried_for_own_name_is_a_collision() {
        // Ranked #9 for its own name: same-named siblings bury it. Even
        // undocumented, the fix is disambiguation, NOT a doc line.
        let (cause, sev, _) = classify(&sig(false, false, true, Some(9), false, None));
        assert_eq!(cause, Cause::Collision);
        assert!(sev >= 2);
        assert!(cause.fix().contains("rename") || cause.fix().contains("distinctive"));
    }

    #[test]
    fn missing_own_name_entirely_is_worst_collision() {
        let (cause, sev, _) = classify(&sig(true, false, false, None, true, Some(1)));
        assert_eq!(cause, Cause::Collision);
        assert_eq!(
            sev, 3,
            "a total self-name miss is the most severe collision"
        );
    }

    #[test]
    fn distinctive_name_but_intent_miss_is_a_vocab_gap() {
        // Findable by its own name (#1) but an intent query that avoids the name
        // misses it → the concept↔code bridge is weak → add a doc.
        let (cause, _, _) = classify(&sig(false, false, false, Some(1), true, Some(9)));
        assert_eq!(cause, Cause::VocabGap);
        assert!(cause.fix().contains("doc"));
    }

    #[test]
    fn isolated_undocumented_is_isolation() {
        let (cause, _, _) = classify(&sig(false, true, false, Some(1), false, None));
        assert_eq!(cause, Cause::Isolation);
        assert!(cause.fix().contains("graph") || cause.fix().contains("doc"));
    }

    #[test]
    fn distinctive_documented_findable_is_ok() {
        let (cause, _, _) = classify(&sig(true, false, false, Some(1), true, Some(1)));
        assert_eq!(cause, Cause::Ok);
        assert!(cause.fix().is_empty());
    }

    #[test]
    fn undocumented_but_findable_is_not_a_finding() {
        // No doc, but ranks well for both name and intent, connected: fine.
        let (cause, _, _) = classify(&sig(false, false, false, Some(1), true, Some(2)));
        assert_eq!(cause, Cause::Ok);
    }

    #[test]
    fn tokenless_short_name_is_unnameable_not_collision() {
        // A 1-2 char name (e.g. `Id`, `A`) tokenizes to nothing, so it can't be
        // queried by name at all. With no concept bridge either, that is its own
        // cause (rename), not a "collision".
        let s = Signals {
            has_doc: false,
            isolated: false,
            generic_name: false,
            self_name_rank: None,
            intent_probed: false,
            intent_rank: None,
            nameable: false,
        };
        let (cause, _, reasons) = classify(&s);
        assert_eq!(cause, Cause::Unnameable);
        assert!(cause.fix().contains("descriptive") || cause.fix().contains("rename"));
        assert!(reasons.iter().any(|r| r.contains("queryable tokens")));
    }

    #[test]
    fn tokenless_name_saved_by_a_doc_bridge_is_ok() {
        // Same short name, but a doc-derived intent query finds it → not a finding.
        let s = Signals {
            has_doc: true,
            isolated: false,
            generic_name: false,
            self_name_rank: None,
            intent_probed: true,
            intent_rank: Some(1),
            nameable: false,
        };
        assert_eq!(classify(&s).0, Cause::Ok);
    }

    // --- intent derivation ---

    #[test]
    fn derive_intent_strips_own_name_tokens() {
        let q = derive_intent(
            "stem_variants",
            Some("Matches plural and past tense word endings from a query."),
            None,
        )
        .expect("intent should derive");
        assert!(!q.contains("stem"), "own tokens must be stripped: {q}");
        assert!(
            q.contains("plural") && q.contains("tense"),
            "intent words kept: {q}"
        );
    }

    #[test]
    fn derive_intent_none_without_description() {
        assert!(derive_intent("foo_bar", None, None).is_none());
    }

    // --- audit() end-to-end (loose: exercises real extraction + retrieval) ---

    #[test]
    fn audit_runs_and_leaves_a_clean_symbol_ok() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            r#"
            /// Compute the blake3 fingerprint of a directory tree for staleness.
            pub fn fingerprint_directory_tree() -> u64 { helper_seed() }
            fn helper_seed() -> u64 { 0 }
            "#,
        )
        .unwrap();
        let g = extract_dir(dir.path(), "t", Some("rust")).unwrap();
        let store = GraphStore::open_in_memory().unwrap();
        store
            .upsert_source("t", "dev", "rust", &g.nodes, &g.edges)
            .unwrap();

        let report = audit(&g, &store, None).unwrap();
        let clean = report
            .iter()
            .find(|s| s.qualified_name.ends_with("fingerprint_directory_tree"))
            .expect("symbol present");
        assert_eq!(clean.cause, Cause::Ok, "reasons: {:?}", clean.reasons);
    }

    #[test]
    fn authored_queries_override_derived_intent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "/// A short doc.\npub fn widget_factory() {}\n",
        )
        .unwrap();
        let g = extract_dir(dir.path(), "t", Some("rust")).unwrap();
        let store = GraphStore::open_in_memory().unwrap();
        store
            .upsert_source("t", "dev", "rust", &g.nodes, &g.edges)
            .unwrap();

        let mut authored = HashMap::new();
        authored.insert(
            "t::widget_factory".to_string(),
            vec!["construct user interface controls".to_string()],
        );
        let report = audit(&g, &store, Some(&authored)).unwrap();
        let s = report
            .iter()
            .find(|s| s.qualified_name.ends_with("widget_factory"))
            .expect("symbol present");
        assert!(
            s.signals.intent_probed,
            "authored query should drive an intent probe"
        );
    }
}
