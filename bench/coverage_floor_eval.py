#!/usr/bin/env python3
"""STRONG lexical floor — multi-token coverage, graded on symbols.

Companion to `grep_floor_eval.py`. That floor is deliberately minimal: it greps a
SINGLE distinctive query token and ranks the enclosing symbols. This floor is the
strongest fair lexical baseline — it scores every symbol by how many of the
query's distinctive tokens appear in the symbol's own source span (coverage),
tie-broken by total occurrences. It uses the frozen snapshot's symbol spans to
map text to symbols, so it is a *symbol-aware* lexical ranker (think ctags/LSP +
grep), NOT plain grep — its purpose is to isolate how much of roux's Hit@10 is
BM25+graph ranking over and above pure lexical coverage, on the identical frozen
gold, buckets, top-10, and `strict_match` predicate used by `heldout_eval.py`.

Result on the frozen sets is roughly half of roux's Hit@10 (see README Benchmarks).

Usage:
  python3 bench/coverage_floor_eval.py                 # all repos with a snapshot + clone
  python3 bench/coverage_floor_eval.py pandas gin      # subset by source_name
  python3 bench/coverage_floor_eval.py --out results   # also write a JSON artifact
"""
from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"
SNAP = BENCH / "snapshots"
sys.path.insert(0, str(BENCH))
from bench_match import strict_match             # noqa: E402
from bench_metrics import metrics_from_ranks     # noqa: E402
from freeze_heldout import STOP, toks            # noqa: E402
from grep_floor_eval import CLONES, load_symbol_spans  # noqa: E402

IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]+")
TOPK = 10


def build_span_index(name: str) -> list[tuple[str, Counter]]:
    """(qualified_name, Counter(identifier tokens in the symbol's source span))
    for every symbol span in the frozen snapshot, read from the pinned clone."""
    clone = Path(CLONES[name])
    spans_by_file = load_symbol_spans(SNAP / f"{name}.sqlite")
    out: list[tuple[str, Counter]] = []
    for rel, spans in spans_by_file.items():
        fp = clone / rel
        if not fp.exists():
            continue
        try:
            lines = fp.read_text(errors="ignore").splitlines()
        except OSError:
            continue
        for start, end, qname in spans:
            s = max(start - 1, 0)
            e = min(end, len(lines))
            if s >= e:
                continue
            out.append((qname, Counter(t.lower() for ln in lines[s:e]
                                       for t in IDENT.findall(ln))))
    return out


def rank(spans: list[tuple[str, Counter]], qtokens: set[str]) -> list[str]:
    scored = []
    for i, (qname, cnt) in enumerate(spans):
        distinct = total = 0
        for w in qtokens:
            c = cnt.get(w, 0)
            if c:
                distinct += 1
                total += c
        if distinct:
            scored.append((-distinct, -total, i, qname))
    scored.sort()
    seen: set[str] = set()
    names: list[str] = []
    for *_, qname in scored:
        if qname in seen:
            continue
        seen.add(qname)
        names.append(qname)
        if len(names) >= TOPK:
            break
    return names


def eval_repo(name: str) -> dict[str, list[int | None]] | None:
    gold_path = BENCH / f"gold_{name}.frozen.json"
    if not gold_path.exists() or not (SNAP / f"{name}.sqlite").exists():
        return None
    if name not in CLONES or not Path(CLONES[name]).exists():
        print(f"[skip] {name}: clone {CLONES.get(name)} missing", file=sys.stderr)
        return None
    spec = json.load(open(gold_path))
    spans = build_span_index(name)
    by_bucket: dict[str, list[int | None]] = defaultdict(list)
    for q in spec["queries"]:
        names = rank(spans, toks(q["query"]) - STOP)
        r = next((i + 1 for i, nm in enumerate(names)
                  if any(strict_match(nm, g) for g in q["gold"])), None)
        by_bucket[q["bucket"]].append(r)
    return by_bucket


def fmt(ranks: list[int | None]) -> str:
    m = metrics_from_ranks(ranks)
    n = len(ranks)
    return (f"n={n:>3}  Hit@1 {m['hit1']:>2}/{n}  Hit@5 {m['hit5']:>2}/{n}  "
            f"Hit@10 {m['hit10']:>2}/{n}  MRR {m['mrr']:.3f}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("repos", nargs="*", help="subset by source_name")
    ap.add_argument("--out", metavar="DIR", help="write a JSON results artifact here")
    args = ap.parse_args()

    want = set(args.repos)
    combined: dict[str, list[int | None]] = defaultdict(list)
    print("=" * 78)
    print("MULTI-TOKEN COVERAGE FLOOR (symbol-aware lexical) — frozen held-out sets.")
    print("Isolates BM25+graph value; NOT a plain-grep competitor. See docstring.")
    print("=" * 78)
    for name in ["ripgrep", "pandas", "gin", "remix", "Marlin"]:
        if want and name not in want:
            continue
        res = eval_repo(name)
        if not res:
            continue
        print(f"\n=== {name} ===")
        for bucket in sorted(res):
            print(f"  {bucket:2}  {fmt(res[bucket])}")
            combined[bucket] += res[bucket]
    print("\n=== combined ===")
    for bucket in sorted(combined):
        print(f"  {bucket:2}  {fmt(combined[bucket])}")
    allr = [r for b in combined.values() for r in b]
    if allr:
        m = metrics_from_ranks(allr)
        print(f"  all  n={len(allr)}  Hit@10 {m['hit10']}/{len(allr)} "
              f"({m['hit10'] / len(allr) * 100:.1f}%)  MRR {m['mrr']:.3f}")
    if args.out:
        out_dir = Path(args.out)
        out_dir.mkdir(parents=True, exist_ok=True)
        ts = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M")
        path = out_dir / f"coverage_floor_{ts}.json"
        payload = {"generated": ts,
                   "buckets": {b: metrics_from_ranks(r) | {"n": len(r)}
                               for b, r in combined.items()}}
        path.write_text(json.dumps(payload, indent=1))
        print(f"\nwrote {path}")


if __name__ == "__main__":
    main()
