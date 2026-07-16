#!/usr/bin/env python3
"""Fair, symbol-graded grep-savvy retrieval FLOOR for the frozen held-out sets.

The honest question roux must answer is not "does it beat a whole-query grep"
(a strawman: `rg` on a full natural-language issue title matches almost nothing)
but "does roux's ranking beat what a grep-savvy engineer actually does" — pick
the most distinctive word in the question, `rg` for it, and read the enclosing
symbols. This harness builds that floor and grades it on SYMBOLS, exactly like
roux (same frozen gold, same word-boundary `strict_match`, same top-10, same
MRR), so the two columns are directly comparable.

Two gold-BLIND token-selection rules are reported side by side so no single
tuned heuristic can masquerade as an oracle:
  - longest : the longest distinctive query token (tie -> rarer, then alpha)
  - rarest  : the distinctive query token with the fewest in-source matches
"Distinctive" = freeze_heldout.toks(query) minus its STOP list — the exact token
set that assigned each query its S/L bucket, so the floor is consistent with the
buckets it is scored against. The retired whole-query strawman is shown only as a
dim context line, never as a cited baseline.

grep hits map to the tightest enclosing symbol via the frozen snapshot's node
spans (file_path + [start_line, end_line]); the snapshot and clone are pinned to
the same ref, so line ranges line up.

Usage:
  python3 bench/grep_floor_eval.py            # all repos with a snapshot + clone
  python3 bench/grep_floor_eval.py ripgrep    # subset by name
"""
from __future__ import annotations

import bisect
import json
import sqlite3
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_match import strict_match  # noqa: E402  # type: ignore[import-not-found]
from bench_metrics import first_hit_rank, metrics_from_ranks  # noqa: E402  # type: ignore[import-not-found]
from freeze_heldout import STOP, toks  # noqa: E402  # type: ignore[import-not-found]

ROOT = Path(__file__).resolve().parent.parent
ROUX = ROOT / "target" / "release" / "roux"
BENCH = ROOT / "bench"
SNAP = BENCH / "snapshots"
TOPK = 10

# name -> source clone (pinned to the same ref as the snapshot). The gold set is
# bench/gold_<name>.frozen.json and the index is bench/snapshots/<name>.sqlite.
CLONES = {
    "ripgrep": "/tmp/rg-bench",
    "pandas": "/tmp/pandas-bench",
    "gin": "/tmp/gin-bench",
    "remix": "/tmp/remix-bench",
    "Marlin": "/tmp/marlin-bench",
}


def load_symbol_spans(snapshot: Path) -> dict[str, list[tuple[int, int, str]]]:
    """Per-file symbol spans from the snapshot, each (start_line, end_line,
    qualified_name), sorted by start_line. File nodes (end_line == 0) carry no
    span and are excluded — a grep hit only counts when it lands inside a real
    symbol, which is precisely what "the enclosing symbol" means."""
    con = sqlite3.connect(f"file:{snapshot}?mode=ro", uri=True)
    by_file: dict[str, list[tuple[int, int, str]]] = defaultdict(list)
    for fp, start, end, qn in con.execute(
        "SELECT file_path, start_line, end_line, qualified_name FROM nodes WHERE end_line > 0"
    ):
        by_file[fp].append((start, end, qn))
    con.close()
    for spans in by_file.values():
        spans.sort()
    return by_file


def enclosing_symbol(spans: list[tuple[int, int, str]], line: int) -> str | None:
    """Tightest symbol whose [start, end] contains `line`. spans is sorted by
    start_line; scan the candidates that begin at or before the line and keep
    the smallest span that still covers it."""
    idx = bisect.bisect_right(spans, (line, 1 << 62, ""))
    best: tuple[int, str] | None = None
    for start, end, qn in spans[:idx]:
        if start <= line <= end:
            span = end - start
            if best is None or span < best[0]:
                best = (span, qn)
    return best[1] if best else None


def rg_matches(token: str, clone: str) -> list[tuple[str, int]]:
    """(relative_path, line_number) for every case-insensitive literal hit of
    `token` in the clone. Respects .gitignore and skips binaries (rg default)."""
    proc = subprocess.run(
        ["rg", "-i", "--fixed-strings", "--line-number", "--no-heading",
         "--color=never", "--", token, "."],
        cwd=clone, capture_output=True, text=True, errors="replace", check=False,
    )
    out: list[tuple[str, int]] = []
    for row in proc.stdout.splitlines():
        # format: path:line:content
        parts = row.split(":", 2)
        if len(parts) < 3:
            continue
        path, lineno = parts[0], parts[1]
        if not lineno.isdigit():
            continue
        out.append((path.removeprefix("./"), int(lineno)))
    return out


def rank_symbols(matches: list[tuple[str, int]], spans_by_file: dict) -> list[str]:
    """Map grep hits to enclosing symbols and rank them the way a person scanning
    grep output would: most hits first, ties broken by earliest appearance."""
    hits: dict[str, int] = defaultdict(int)
    first_seen: dict[str, tuple[str, int]] = {}
    for path, line in matches:
        spans = spans_by_file.get(path)
        if not spans:
            continue
        qn = enclosing_symbol(spans, line)
        if qn is None:
            continue
        hits[qn] += 1
        if qn not in first_seen:
            first_seen[qn] = (path, line)
    return sorted(hits, key=lambda q: (-hits[q], first_seen[q]))


def candidates(query: str) -> list[str]:
    """Distinctive query tokens (freeze buckets are assigned from exactly this
    set), longest first for stable ordering."""
    return sorted(toks(query) - STOP, key=lambda t: (-len(t), t))


def roux_ranked(snapshot: Path, query: str) -> list[str]:
    proc = subprocess.run(
        [str(ROUX), "query", "--db", str(snapshot), "--format", "json",
         "--top", str(TOPK), "--", query],
        capture_output=True, text=True, check=False,
    )
    try:
        data = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return []
    names = [s.get("qualified_name") or s.get("name", "") for s in data.get("symbols", [])]
    return names[:TOPK]


def rank_of(names: list[str], gold: list[str]) -> int | None:
    ranks = [i + 1 for i, nm in enumerate(names) if any(strict_match(nm, g) for g in gold)]
    return ranks[0] if ranks else None


def eval_repo(name: str) -> dict[str, dict[str, list[int | None]]] | None:
    gold_path = BENCH / f"gold_{name}.frozen.json"
    snapshot = SNAP / f"{name}.sqlite"
    clone = CLONES.get(name)
    if not gold_path.exists() or not snapshot.exists() or not clone or not Path(clone).is_dir():
        print(f"— skip {name} (missing gold/snapshot/clone)")
        return None

    spec = json.loads(gold_path.read_text())
    spans_by_file = load_symbol_spans(snapshot)

    # arm -> bucket -> list of first-hit ranks (None = miss)
    arms = ("roux", "grep-longest", "grep-rarest")
    results: dict[str, dict[str, list[int | None]]] = {a: defaultdict(list) for a in arms}

    for q in spec["queries"]:
        query, gold, bucket = q["query"], q["gold"], q["bucket"]

        results["roux"][bucket].append(rank_of(roux_ranked(snapshot, query), gold))

        cands = candidates(query)
        # Cache each candidate's match list once; both arms reuse it.
        match_lists = {t: rg_matches(t, clone) for t in cands}

        longest_tok = cands[0] if cands else None
        nonzero = {t: m for t, m in match_lists.items() if m}
        rarest_tok = min(nonzero, key=lambda t: (len(nonzero[t]), -len(t), t)) if nonzero else None

        for arm, tok in (("grep-longest", longest_tok), ("grep-rarest", rarest_tok)):
            if tok is None:
                results[arm][bucket].append(None)
            else:
                names = rank_symbols(match_lists[tok], spans_by_file)[:TOPK]
                results[arm][bucket].append(rank_of(names, gold))
    return results


def fmt(ranks: list[int | None]) -> str:
    m = metrics_from_ranks(ranks)
    n = m["n"]
    return (f"n={n:3}  Hit@1 {m['hit1']:>2}/{n}  Hit@5 {m['hit5']:>2}/{n}  "
            f"Hit@10 {m['hit10']:>2}/{n}  MRR {m['mrr']:.3f}")


def main() -> None:
    want = set(sys.argv[1:])
    names = [n for n in CLONES if not want or n in want]
    combined: dict[str, dict[str, list[int | None]]] = {
        a: defaultdict(list) for a in ("roux", "grep-longest", "grep-rarest")
    }

    print("=" * 78)
    print("GREP-SAVVY FLOOR vs roux — frozen held-out sets, graded on symbols.")
    print("Whole-query grep is a retired strawman and is NOT reported as a baseline.")
    print("=" * 78)

    for name in names:
        res = eval_repo(name)
        if not res:
            continue
        print(f"\n=== {name} ===")
        for bucket, label in (("L", "L (control · lexical)"), ("S", "S (vocab gap)")):
            if not res["roux"].get(bucket):
                continue
            print(f"  {label}")
            for arm in ("roux", "grep-longest", "grep-rarest"):
                ranks = res[arm][bucket]
                combined[arm][bucket].extend(ranks)
                print(f"    {arm:14} {fmt(ranks)}")

    print("\n=== combined (all sets) ===")
    for bucket, label in (("L", "L (control · lexical)"), ("S", "S (vocab gap)")):
        if not combined["roux"].get(bucket):
            continue
        print(f"  {label}")
        for arm in ("roux", "grep-longest", "grep-rarest"):
            print(f"    {arm:14} {fmt(combined[arm][bucket])}")


if __name__ == "__main__":
    main()
