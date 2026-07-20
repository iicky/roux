#!/usr/bin/env python3
"""Score the FROZEN, blind, held-out query sets — reported SEPARATELY from the CI gate.

This is the anti-Goodhart companion to the persona CI gate. The gate
set (tests/bench_personas.rs / baseline-metrics.json) is lexically friendly and
was edited post-hoc after observing misses; it can regress-trip but cannot
honestly source public ranking claims. The held-out sets here are harvested
BLIND from a persona's own closed-issue titles (real user queries), pinned to
the fix commit's touched symbols, then frozen. NEVER tune against them.

Reads every bench/gold_*.frozen.json, runs `roux query --db <snapshot>` against
the frozen index snapshot, and reports Hit@1/5/10 + MRR split by bucket:

  S = no distinctive query<->gold lexical overlap (pure vocab/semantic gap —
      the real test; BM25 cannot seed it, only graph/rerank can reach it)
  L = distinctive lexical overlap (control; must score near-perfect)

Match predicate = bench_match.strict_match (word-boundary tokens, shared with
rank_eval.py / snapshot_bench.py / tests/common/mod.rs) against the full gold
qualified_name: the gold symbol itself must appear in the top-K.

Usage:
  python3 bench/heldout_eval.py                 # all frozen sets
  python3 bench/heldout_eval.py ripgrep         # subset by source_name
  python3 bench/heldout_eval.py --show          # also print per-query ranks
  python3 bench/heldout_eval.py --out results   # also write a JSON results artifact
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_metrics import (  # noqa: E402  # type: ignore[import-not-found]
    first_hit_rank,
    metrics_from_ranks as bucket_metrics,
)

ROOT = Path(__file__).resolve().parent.parent
ROUX = ROOT / "target" / "release" / "roux"
BENCH = ROOT / "bench"
SNAPSHOTS = BENCH / "snapshots"
TOPK = 10


def resolve_index(spec: dict) -> Path | None:
    """Locate the frozen index snapshot, tolerant of the machine that froze it.

    The frozen JSON records an absolute index_path; fall back to a repo-relative
    bench/snapshots/<source_name>.sqlite (or the recorded basename) so the report
    runs anywhere the snapshots are checked out.
    """
    candidates = []
    raw = spec.get("index_path")
    if raw:
        candidates.append(Path(raw))
        candidates.append(SNAPSHOTS / Path(raw).name)
    name = spec.get("source_name")
    if name:
        candidates.append(SNAPSHOTS / f"{name}.sqlite")
    for c in candidates:
        if c.exists():
            return c
    return None


def run_query(index: Path, query: str, top: int = TOPK) -> list[str]:
    cmd = [str(ROUX), "query", query, "--db", str(index), "--format", "json", "--top", str(top)]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120, check=False)
        data = json.loads(proc.stdout)
    except (json.JSONDecodeError, OSError, subprocess.SubprocessError):
        return []
    names = [s.get("qualified_name") or s.get("name", "") for s in data.get("symbols", [])]
    return names[:top]


def eval_spec(spec: dict, show: bool) -> dict | None:
    name = spec.get("source_name", "?")
    index = resolve_index(spec)
    if index is None:
        print(f"— skip {name}: no index snapshot "
              f"(looked for {spec.get('index_path')} and {SNAPSHOTS}/{name}.sqlite)")
        return None

    fp = spec.get("freeze_provenance", {})
    badge = "AUTO-FROZEN · review pending" if fp.get("auto_frozen") else "human-reviewed"
    print(f"\n=== HELD-OUT · {name} @ {spec.get('pinned_ref', '?')} "
          f"(frozen {fp.get('frozen', '?')}, roux {fp.get('roux_commit', '?')} · {badge}) ===")
    print(f"    index: {index}")

    by_bucket: dict[str, list[int | None]] = defaultdict(list)
    per_query: list[dict] = []
    for q in spec["queries"]:
        names = run_query(index, q["query"])
        rank = first_hit_rank(names, q["gold"])
        by_bucket[q["bucket"]].append(rank)
        per_query.append({"id": q["id"], "bucket": q["bucket"], "rank": rank,
                          "rank_at_freeze_substr": q.get("roux_rank_at_freeze")})
        if show:
            frozen = q.get("roux_rank_at_freeze")
            cur = f"#{rank}" if rank else "MISS"
            # roux_rank_at_freeze was recorded under the harvester's short-name
            # SUBSTRING predicate, not strict_match — provenance only, NOT drift.
            note = f"  [freeze~#{frozen} substr]" if frozen else ""
            print(f"  [{cur:>5}] {q['bucket']} {q['id']:8} {q['query'][:52]:52}"
                  f" -> {','.join(g.split('::')[-1] for g in q['gold'])[:40]}{note}")

    labels = {"S": "S (THE TEST · vocab gap)", "L": "L (control · lexical)"}
    metrics = {}
    for bucket in sorted(by_bucket):
        m = bucket_metrics(by_bucket[bucket])
        metrics[bucket] = m
        label = labels.get(bucket, bucket)
        print(f"  {label:26} n={m['n']:3}  Hit@1 {m['hit1']:2}/{m['n']:<2}  "
              f"Hit@5 {m['hit5']:2}/{m['n']:<2}  Hit@10 {m['hit10']:2}/{m['n']:<2}  MRR {m['mrr']:.3f}")
    overall = bucket_metrics([r for rs in by_bucket.values() for r in rs])
    metrics["overall"] = overall
    print(f"  {'overall':26} n={overall['n']:3}  Hit@1 {overall['hit1']:2}/{overall['n']:<2}  "
          f"Hit@5 {overall['hit5']:2}/{overall['n']:<2}  Hit@10 {overall['hit10']:2}/{overall['n']:<2}  "
          f"MRR {overall['mrr']:.3f}")
    return {"source_name": name, "pinned_ref": spec.get("pinned_ref"),
            "index_path": str(index), "metrics": metrics, "queries": per_query}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("sources", nargs="*", help="filter by source_name substring")
    ap.add_argument("--show", action="store_true", help="print per-query ranks (strict predicate)")
    ap.add_argument("--out", default="", help="write a JSON results artifact under this dir")
    args = ap.parse_args()

    if not ROUX.exists():
        sys.exit("roux release binary missing (cargo build --release)")

    files = sorted(BENCH.glob("gold_*.frozen.json"))
    if args.sources:
        files = [f for f in files if any(s.lower() in f.stem.lower() for s in args.sources)]
    if not files:
        sys.exit("no frozen held-out sets found (bench/gold_*.frozen.json)")

    print("=" * 78)
    print("HELD-OUT EVAL — blind, frozen. NOT the CI gate. NEVER tune against this set.")
    print("Report ALONGSIDE the persona gate (baseline-metrics.json), never inside it.")
    print("=" * 78)

    results = [r for r in (eval_spec(json.loads(f.read_text()), args.show) for f in files) if r]
    if not results:
        sys.exit("no held-out sets scored — every frozen spec was skipped "
                 "(missing index snapshots under bench/snapshots/). Nothing reported.")

    if len(results) > 1:
        print("\n=== HELD-OUT · combined (all frozen sets) ===")
        combined: dict[str, list[int | None]] = defaultdict(list)
        for r in results:
            for q in r["queries"]:
                combined[q["bucket"]].append(q["rank"])
        for bucket in sorted(combined):
            m = bucket_metrics(combined[bucket])
            print(f"  {bucket:6} n={m['n']:3}  Hit@1 {m['hit1']:3}/{m['n']:<3}  "
                  f"Hit@5 {m['hit5']:3}/{m['n']:<3}  Hit@10 {m['hit10']:3}/{m['n']:<3}  MRR {m['mrr']:.3f}")
        allr = bucket_metrics([r for rs in combined.values() for r in rs])
        print(f"  {'all':6} n={allr['n']:3}  Hit@1 {allr['hit1']:3}/{allr['n']:<3}  "
              f"Hit@5 {allr['hit5']:3}/{allr['n']:<3}  Hit@10 {allr['hit10']:3}/{allr['n']:<3}  "
              f"MRR {allr['mrr']:.3f}")

    if args.out:
        outdir = Path(args.out)
        outdir.mkdir(parents=True, exist_ok=True)
        stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
        path = outdir / f"heldout_{stamp}.json"
        path.write_text(json.dumps(
            {"_comment": "HELD-OUT eval results — separate from the CI gate. Never tune against.",
             "generated": stamp, "sets": results}, indent=2) + "\n")
        print(f"\nwrote results -> {path}")


if __name__ == "__main__":
    main()
