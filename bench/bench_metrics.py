"""Shared ranking metrics for the Python bench harnesses.

The scoring primitive is ``first_hit_rank`` — the 1-based rank of the first
ranked symbol that strictly matches any gold (or ``None``). Hit@K and MRR derive
from it. Mirrors the metric functions in ``tests/common/mod.rs`` so the Rust CI
gate and the Python loops report the same numbers; keep the two in sync.
"""
from __future__ import annotations

from bench_match import strict_match  # type: ignore[import-not-found]


def first_hit_rank(names: list[str], expected: list[str]) -> int | None:
    """1-based rank of the first name strictly matching any gold, else ``None``."""
    for i, n in enumerate(names, start=1):
        if any(strict_match(n, e) for e in expected):
            return i
    return None


def hit_at_k(results: list[tuple[list[str], list[str]]], k: int) -> float:
    """Fraction of queries whose first matching result lands within the top K."""
    if not results:
        return 0.0
    hits = sum(
        1
        for names, expected in results
        if (r := first_hit_rank(names, expected)) is not None and r <= k
    )
    return hits / len(results)


def mrr(results: list[tuple[list[str], list[str]]]) -> float:
    """Mean reciprocal rank of the first matching result per query."""
    if not results:
        return 0.0
    total = sum(
        1.0 / r
        for names, expected in results
        if (r := first_hit_rank(names, expected)) is not None
    )
    return total / len(results)


def metrics_from_ranks(ranks: list[int | None]) -> dict:
    """Hit@1/5/10 (counts) and MRR over a list of first-hit ranks (None = miss)."""
    n = len(ranks)
    hit = lambda k: sum(1 for r in ranks if r is not None and r <= k)  # noqa: E731
    m = sum(1.0 / r for r in ranks if r is not None) / n if n else 0.0
    return {"n": n, "hit1": hit(1), "hit5": hit(5), "hit10": hit(10), "mrr": m}
