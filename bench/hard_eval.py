#!/usr/bin/env python3
"""Eval roux on the authored vocabulary-mismatch sets, per bucket.

Reads bench/hard_queries_<repo>.json, runs `roux query` against the local index
(same invocation as bench/rank_eval.py), and reports Hit@K / MRR split by bucket.
The S bucket is the only one that tests embeddings: zero query-lexical overlap
with the gold's indexed text, so BM25 cannot seed it — it can only be reached by
the graph or by dense rerank. L/B are lexical controls and must score ~perfect.

Usage:
  python3 bench/hard_eval.py                      # all hard_queries_*.json
  python3 bench/hard_eval.py marlin               # one set
  python3 bench/hard_eval.py --show               # also print per-query ranks
"""
from __future__ import annotations

import json
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUX = ROOT / "target" / "release" / "roux"
BENCH = ROOT / "bench"


def run_query(path: str, query: str, top: int = 10) -> list[str]:
    cmd = [str(ROUX), "query", query, "--local", "--format", "json", "--top", str(top)]
    try:
        proc = subprocess.run(
            cmd, cwd=path, capture_output=True, text=True, timeout=120, check=False,
        )
        data = json.loads(proc.stdout)
    except (json.JSONDecodeError, OSError, subprocess.SubprocessError):
        return []
    return [s.get("qualified_name") or s.get("name", "") for s in data.get("symbols", [])]


def first_hit_rank(names: list[str], gold: list[str]) -> int | None:
    for i, n in enumerate(names):
        if any(g.lower() in n.lower() for g in gold):
            return i + 1
    return None


def eval_set(spec: dict, show: bool) -> dict:
    path = spec["index_path"]
    if not (Path(path) / ".roux" / "db.sqlite").exists():
        print(f"— skip {spec['source_name']} (no index at {path}; run bench/build_persona_indexes.sh)")
        return {}
    print(f"\n=== {spec['source_name']} [lexical+graph] ===")
    by_bucket: dict[str, list[tuple[int | None, dict]]] = defaultdict(list)
    for q in spec["queries"]:
        names = run_query(path, q["query"])
        rank = first_hit_rank(names, q["gold"])
        by_bucket[q["bucket"]].append((rank, q))
        if show:
            tag = f"#{rank}" if rank else "MISS"
            print(f"  [{tag:>4}] {q['id']:3} {q['query'][:44]:44} -> {','.join(q['gold'])}")
    for bucket in sorted(by_bucket):
        rows = by_bucket[bucket]
        n = len(rows)
        h1 = sum(1 for r, _ in rows if r == 1)
        h5 = sum(1 for r, _ in rows if r and r <= 5)
        h10 = sum(1 for r, _ in rows if r and r <= 10)
        mrr = sum(1.0 / r for r, _ in rows if r) / n if n else 0.0
        label = {"S": "S (THE TEST)", "L": "L (control)", "B": "B (control)"}.get(bucket, bucket)
        print(f"  {label:14} n={n:2}  Hit@1 {h1}/{n}  Hit@5 {h5}/{n}  Hit@10 {h10}/{n}  MRR {mrr:.3f}")
    return by_bucket


def main() -> None:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    show = "--show" in sys.argv
    files = sorted(BENCH.glob("hard_queries_*.json"))
    if args:
        files = [f for f in files if any(a.lower() in f.stem.lower() for a in args)]
    for f in files:
        spec = json.loads(f.read_text())
        eval_set(spec, show)


if __name__ == "__main__":
    main()
