#!/usr/bin/env python3
"""Compare ripgrep against roux on the same self-benchmark query set.

For each natural-language query, pick the single most distinctive token an
agent might plausibly grep for. Run ripgrep against src/, take the top K
lines as returned, and check whether any expected symbol name appears as
a substring — same rule roux's own bench uses.

This is intentionally charitable to grep: the query term is hand-picked to
be the term an agent would likely try first.
"""

from __future__ import annotations
import subprocess
import sys
from dataclasses import dataclass

REPO_ROOT = subprocess.check_output(
    ["git", "rev-parse", "--show-toplevel"], text=True
).strip()


# Mirrors ROUX_QUERIES in tests/bench_retrieval.rs. Two grep-term strategies:
#  - savvy:  best symbol-like token an agent might infer (charitable to grep,
#            sometimes essentially the answer)
#  - naive:  single most distinctive *content word from the NL query itself*,
#            no peeking at the code — what an agent's first grep would look like
@dataclass
class Q:
    nl: str
    savvy: str
    naive: str
    expected: list[str]


QUERIES: list[Q] = [
    Q("download a crate from crates.io", "download_crate", "crates.io",
      ["download_crate", "validate_crate_name"]),
    Q("search the graph for matching nodes", "GraphStore", "graph",
      ["search", "GraphStore"]),
    Q("parse source code with tree-sitter", "tree-sitter", "tree-sitter",
      ["extract_from_source", "extract_node", "extract_dir"]),
    Q("personalized pagerank ranking", "pagerank", "pagerank",
      ["personalized_pagerank", "rank_subgraph"]),
    Q("store nodes and edges in sqlite", "upsert", "sqlite",
      ["upsert_source", "GraphStore"]),
    Q("extract functions and classes from source", "extract_from_source", "extract",
      ["extract_from_source", "extract_tags"]),
    Q("resolve unresolved references", "resolve_references", "resolve",
      ["resolve_references"]),
    Q("escape query string for fts matching", "fts_query", "fts",
      ["fts_query_escape", "tokenize_for_fts"]),
    Q("detect language from file extension", "detect_language", "language",
      ["detect_language", "get_ts_language"]),
    Q("configuration and store path", "resolve_store_path", "config",
      ["resolve_store_path", "Config"]),
    Q("extract markdown documentation sections", "markdown", "markdown",
      ["extract_markdown_doc", "flush_doc_section"]),
    Q("extract decorator edges from Python", "decorator", "decorator",
      ["extract_decorator_edges", "decorates"]),
    Q("infer which tests cover which functions", "infer_test", "infer",
      ["infer_test_edges", "extract_tested_name"]),
    Q("detect function visibility public private", "visibility", "visibility",
      ["detect_visibility"]),
    Q("walk directory tree for source files", "walk_dir", "walk",
      ["walk_dir"]),
    Q("extract structs and enums from source", "extract_from_source", "struct",
      ["extract_from_source", "extract_tags"]),
    Q("extract class and function nodes from JS", "extract_from_source", "class",
      ["extract_from_source", "extract_tags"]),
    Q("Go function and method extraction", "extract_from_source", "method",
      ["extract_from_source", "extract_tags"]),
    Q("backtick references in markdown", "backtick", "backtick",
      ["extract_backtick_refs"]),
    Q("HTTP route handler detection", "route", "route",
      ["extract_route_registrations", "routes"]),
    Q("raise throw error detection", "raise_edges", "raise",
      ["extract_raise_edges"]),
    Q("inheritance class extends parent", "extends", "inheritance",
      ["extract_relationship_edges", "inherits"]),
    Q("blake3 hash of source text", "blake3", "blake3",
      ["content_hash", "build_body"]),
    Q("file node creation from path", "make_file_node", "file_node",
      ["make_file_node"]),
    Q("remove source delete from index", "remove_source", "remove",
      ["remove_source"]),
]


def run_rg(term: str, limit: int) -> list[str]:
    """Run ripgrep against src/, return up to `limit` result lines in rg's
    default order (file alpha, line ascending)."""
    try:
        out = subprocess.check_output(
            ["rg", "--line-number", "--color=never", term, "src/"],
            cwd=REPO_ROOT,
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return []  # no matches
    return out.splitlines()[:limit]


def hit_at_k(lines: list[str], expected: list[str], k: int) -> bool:
    """Any of the top-k lines contain any expected symbol as substring."""
    return any(any(e in line for e in expected) for line in lines[:k])


def mrr_rank(lines: list[str], expected: list[str]) -> int | None:
    """Rank (1-indexed) of first line containing an expected symbol, or None."""
    for i, line in enumerate(lines, start=1):
        if any(e in line for e in expected):
            return i
    return None


def evaluate(mode: str) -> tuple[dict[int, int], float, list[tuple[str, str, int | None]]]:
    k_values = [1, 5, 10]
    fetch_limit = 10  # same as roux's top=10
    hits = {k: 0 for k in k_values}
    rr_sum = 0.0
    per_query: list[tuple[str, str, int | None]] = []
    for q in QUERIES:
        term = q.savvy if mode == "savvy" else q.naive
        lines = run_rg(term, fetch_limit)
        rank = mrr_rank(lines, q.expected)
        per_query.append((q.nl, term, rank))
        for k in k_values:
            if hit_at_k(lines, q.expected, k):
                hits[k] += 1
        if rank:
            rr_sum += 1.0 / rank
    mrr = rr_sum / len(QUERIES)
    return hits, mrr, per_query


def main() -> int:
    n = len(QUERIES)

    print("═══ ripgrep vs roux self-bench (25 queries) ═══\n")
    print("Two grep strategies:")
    print("  savvy:  agent guesses a symbol-like token (charitable)")
    print("  naive:  agent greps a word from the NL query (realistic)\n")

    for mode, label in [("savvy", "savvy (charitable)"), ("naive", "naive (realistic)")]:
        hits, mrr, per_query = evaluate(mode)
        print(f"── grep, {label} ──")
        print(f"  {'query':<48} {'term':<22}  rank")
        print("  " + "─" * 80)
        for nl, term, rank in per_query:
            rank_str = f"@{rank}" if rank else "miss"
            mark = "✓" if rank else "✗"
            q_short = (nl[:45] + "...") if len(nl) > 45 else nl
            print(f"  {mark} {q_short:<48} {term:<22} {rank_str:>5}")
        for k in [1, 5, 10]:
            print(f"  Hit@{k}: {hits[k]/n*100:>5.1f}%  ({hits[k]}/{n})")
        print(f"  MRR:    {mrr:.3f}\n")

    print("── roux self-bench (from cargo test bench_self_retrieval) ──")
    print(f"  Hit@1:  64.0%  (16/25)")
    print(f"  Hit@5:  92.0%  (23/25)")
    print(f"  Hit@10: 96.0%  (24/25)")
    print(f"  MRR:    0.750")
    return 0


if __name__ == "__main__":
    sys.exit(main())
