#!/usr/bin/env python3
"""Persona-repo grep vs roux comparison.

Parses tests/bench_personas.rs to extract the exact query set roux uses,
then runs `rg` against each persona's directory with the same query string.
Computes Hit@K and MRR per persona and split (agent vs developer).

Uses the whole NL query as the grep pattern — the fairest apples-to-apples
because roux gets the same string and has to do its own term extraction.
Also reports a "best-token" variant where we pick the longest word from the
query (what a savvy agent would try).
"""

from __future__ import annotations
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
PERSONAS_RS = REPO_ROOT / "tests" / "bench_personas.rs"


@dataclass
class PQ:
    persona: str
    path: str
    language: str
    query: str
    expected: list[str]
    mode: str  # "Agent" or "Developer"


def parse_personas() -> list[PQ]:
    text = PERSONAS_RS.read_text()
    # Each Persona block: capture name/path/language and its queries list.
    persona_re = re.compile(
        r'name:\s*"([^"]+)",\s*'
        r'path:\s*"([^"]+)",\s*'
        r'language:\s*"([^"]+)",\s*'
        r'queries:\s*&\[(.*?)\],?\s*\};',
        re.DOTALL,
    )
    pq_re = re.compile(
        r'query:\s*"([^"]+)",\s*'
        r'expected:\s*&\[([^\]]+)\],\s*'
        r'mode:\s*QueryMode::(Agent|Developer)',
    )
    expected_re = re.compile(r'"([^"]+)"')
    out: list[PQ] = []
    for pm in persona_re.finditer(text):
        name, path, lang, body = pm.groups()
        for qm in pq_re.finditer(body):
            q, exp_raw, mode = qm.groups()
            expected = expected_re.findall(exp_raw)
            out.append(PQ(name, path, lang, q, expected, mode))
    return out


def best_token(query: str) -> str:
    """Longest content word from the query — a savvy agent's grep term."""
    stop = {"the", "and", "a", "of", "for", "in", "with", "how", "is", "are",
            "an", "on", "to", "from", "by", "or", "what", "that", "this",
            "does", "do", "can", "when", "where", "which"}
    tokens = re.findall(r"[A-Za-z][A-Za-z_]+", query)
    candidates = [t for t in tokens if t.lower() not in stop]
    if not candidates:
        candidates = tokens or [query]
    return max(candidates, key=len)


def run_rg(pattern: str, path: str, limit: int) -> list[str]:
    try:
        out = subprocess.check_output(
            ["rg", "--line-number", "--color=never", "--fixed-strings", pattern, path],
            text=True,
            stderr=subprocess.DEVNULL,
        )
    except subprocess.CalledProcessError:
        return []
    return out.splitlines()[:limit]


def hit_at_k(lines: list[str], expected: list[str], k: int) -> bool:
    return any(any(e in line for e in expected) for line in lines[:k])


def mrr_rank(lines: list[str], expected: list[str]) -> int | None:
    for i, line in enumerate(lines, start=1):
        if any(e in line for e in expected):
            return i
    return None


def score(queries: list[PQ], strategy: str) -> dict:
    """strategy: 'whole' (full query, FTS-style) or 'best' (longest word)."""
    by_persona: dict[str, dict] = {}
    for pq in queries:
        pattern = pq.query if strategy == "whole" else best_token(pq.query)
        lines = run_rg(pattern, pq.path, 10)
        rank = mrr_rank(lines, pq.expected)

        bucket = by_persona.setdefault(pq.persona, {
            "all": [], "agent": [], "dev": [],
        })
        row = {"query": pq.query, "pattern": pattern, "rank": rank, "expected": pq.expected}
        bucket["all"].append(row)
        (bucket["agent"] if pq.mode == "Agent" else bucket["dev"]).append(row)
    return by_persona


def summarize(rows: list[dict], k_values=(1, 5, 10)) -> dict:
    n = len(rows)
    if n == 0:
        return {"n": 0}
    hits = {k: sum(1 for r in rows if r["rank"] is not None and r["rank"] <= k) for k in k_values}
    mrr = sum(1.0 / r["rank"] for r in rows if r["rank"] is not None) / n
    return {"n": n, **{f"h{k}": hits[k] / n for k in k_values}, "mrr": mrr}


def pct(x: float) -> str:
    return f"{x*100:.1f}%"


def print_table(by_persona: dict, label: str):
    print(f"── {label} ──")
    print(f"  {'persona':<26} {'split':<6} {'n':>3}  {'Hit@1':>6}  {'Hit@5':>6}  {'Hit@10':>7}  {'MRR':>6}")
    print("  " + "─" * 72)
    agg = {"agent": [], "dev": [], "all": []}
    for persona, buckets in by_persona.items():
        for split in ("agent", "dev", "all"):
            rows = buckets[split]
            s = summarize(rows)
            if s["n"] == 0:
                continue
            print(f"  {persona:<26} {split:<6} {s['n']:>3}  "
                  f"{pct(s['h1']):>6}  {pct(s['h5']):>6}  {pct(s['h10']):>7}  {s['mrr']:>6.3f}")
            agg[split].extend(rows)
        print()
    print("  " + "─" * 72)
    for split in ("agent", "dev", "all"):
        s = summarize(agg[split])
        if s["n"] == 0:
            continue
        print(f"  {'AGGREGATE':<26} {split:<6} {s['n']:>3}  "
              f"{pct(s['h1']):>6}  {pct(s['h5']):>6}  {pct(s['h10']):>7}  {s['mrr']:>6.3f}")
    print()


def load_roux_metrics(path: Path) -> dict | None:
    if not path.exists():
        return None
    return json.loads(path.read_text())


def main() -> int:
    queries = parse_personas()
    # Only run against personas whose path exists
    queries = [q for q in queries if Path(q.path).exists()]
    if not queries:
        print("No persona repos found at their expected paths — skipping.")
        return 1

    print(f"Running {len(queries)} persona queries across "
          f"{len({q.persona for q in queries})} personas\n")

    print_table(score(queries, "whole"),
                "grep, full-query pattern (apples-to-apples with roux input)")
    print_table(score(queries, "best"),
                "grep, longest-word heuristic (savvy agent)")

    # roux side
    roux_json = Path("/tmp/roux-persona.json")
    roux = load_roux_metrics(roux_json)
    if roux and roux.get("personas"):
        print("── roux persona bench ──")
        print(f"  {'persona':<26} {'symbols':>8}  {'Hit@1':>6}  {'Hit@10':>7}  "
              f"{'MRR':>6}  {'agent H@1':>10}  {'dev H@1':>9}")
        print("  " + "─" * 80)
        for p in roux["personas"]:
            m = p["metrics"]
            print(f"  {p['name']:<26} {p['node_count']:>8}  "
                  f"{pct(m['h1']):>6}  {pct(m['h10']):>7}  {m['mrr']:>6.3f}  "
                  f"{pct(m['agent_h1']):>10}  {pct(m['dev_h1']):>9}")
        agg = roux.get("aggregate") or {}
        if agg:
            print("\n  AGGREGATE")
            print(f"    agent  Hit@1: {pct(agg['agent_h1'])}  MRR: {agg['agent_mrr']:.3f}")
            print(f"    dev    Hit@1: {pct(agg['dev_h1'])}  MRR: {agg['dev_mrr']:.3f}")
    else:
        print(f"(no roux metrics at {roux_json} — run the persona bench first)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
