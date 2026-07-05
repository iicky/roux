#!/usr/bin/env python3
"""Fast ranking-quality eval for NL-query experiments (issue #3).

Parses the ground-truth persona queries straight out of tests/bench_personas.rs
(so it never drifts from the canonical set), runs `roux query` against the live
local indexes under /tmp/roux-sources, and reports Hit@K / MRR. Decoupled from
cargo: edit the ranker, `cargo build --release`, rerun this — seconds per loop.

Usage:
  python3 bench/rank_eval.py                 # all personas with an index
  python3 bench/rank_eval.py ripgrep pandas  # subset
A hit = any `expected` substring appears in a returned qualified_name (top-K).
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_match import strict_match  # noqa: E402  # type: ignore[import-not-found]

ROOT = Path(__file__).resolve().parent.parent
ROUX = ROOT / "target" / "release" / "roux"
BENCH = ROOT / "tests" / "bench_personas.rs"
TOPK = 10

# ── parse PERSONAS out of the Rust source ───────────────────────────────────
_PERSONA_RE = re.compile(
    r'name:\s*"([^"]+)".*?path:\s*"([^"]+)".*?queries:\s*&\[(.*?)\n\s*\],',
    re.DOTALL,
)
_Q_RE = re.compile(
    r'query:\s*"([^"]+)",\s*expected:\s*&\[([^\]]*)\]',
    re.DOTALL,
)


def parse_personas() -> list[dict]:
    text = BENCH.read_text()
    out = []
    for m in _PERSONA_RE.finditer(text):
        name, path, body = m.group(1), m.group(2), m.group(3)
        queries = []
        for qm in _Q_RE.finditer(body):
            q = qm.group(1)
            expected = re.findall(r'"([^"]+)"', qm.group(2))
            queries.append((q, expected))
        out.append({"name": name, "path": path, "queries": queries})
    return out


def run_query(path: str, query: str, top: int = TOPK) -> list[str]:
    try:
        proc = subprocess.run(
            [str(ROUX), "query", query, "--local", "--format", "json", "--top", str(top)],
            cwd=path, capture_output=True, text=True, timeout=60, check=False,
        )
        data = json.loads(proc.stdout)
    except (json.JSONDecodeError, OSError, subprocess.SubprocessError):
        return []
    return [s.get("qualified_name") or s.get("name", "") for s in data.get("symbols", [])]


def first_hit_rank(names: list[str], expected: list[str]) -> int | None:
    for i, n in enumerate(names):
        if any(strict_match(n, e) for e in expected):
            return i + 1
    return None


def main() -> None:
    want = set(sys.argv[1:])
    personas = parse_personas()
    grand_h1 = grand_h5 = grand_h10 = grand_n = 0
    grand_mrr = 0.0
    for p in personas:
        if want and not any(w in p["name"] or w in p["path"] for w in want):
            continue
        if not (Path(p["path"]) / ".roux" / "db.sqlite").exists():
            print(f"— skip {p['name']} (no index)")
            continue
        print(f"\n=== {p['name']} ===")
        h1 = h5 = h10 = 0
        mrr = 0.0
        for q, expected in p["queries"]:
            names = run_query(p["path"], q)
            rank = first_hit_rank(names, expected)
            if rank:
                h10 += rank <= 10
                h5 += rank <= 5
                h1 += rank == 1
                mrr += 1.0 / rank
            tag = f"#{rank}" if rank else "MISS"
            print(f"  [{tag:>4}] {q[:46]:46}  exp={','.join(expected)}")
        n = len(p["queries"])
        print(f"  → Hit@1 {h1}/{n}  Hit@5 {h5}/{n}  Hit@10 {h10}/{n}  MRR {mrr/n:.3f}")
        grand_h1 += h1; grand_h5 += h5; grand_h10 += h10; grand_n += n; grand_mrr += mrr
    if grand_n:
        print(f"\n=== OVERALL ({grand_n} q) ===")
        print(f"  Hit@1 {grand_h1}/{grand_n} ({grand_h1/grand_n:.0%})  "
              f"Hit@5 {grand_h5}/{grand_n} ({grand_h5/grand_n:.0%})  "
              f"Hit@10 {grand_h10}/{grand_n} ({grand_h10/grand_n:.0%})  "
              f"MRR {grand_mrr/grand_n:.3f}")


if __name__ == "__main__":
    main()
