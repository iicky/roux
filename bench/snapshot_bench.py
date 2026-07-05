#!/usr/bin/env python3
"""PR regression-gate bench against pre-built snapshot DBs.

Mirrors tests/bench_personas.rs but skips extraction: opens already-built
roux artifact DBs (one per persona) and runs the persona query suite via
`roux query --db ... --format json`. Emits the same JSON shape the nightly
bench writes (h1/h5/h10/mrr per persona + aggregate agent/dev splits) and
diffs the result against bench/baseline-metrics.json.

Exits 1 on regression. Tolerance bands chosen after the 2026-04-23 findings
(see roux-d4w): Hit@1 ties grep on agent queries, so dev_mrr and h10 carry
the signal.

Usage:
    python bench/snapshot_bench.py \
        --db-dir snapshots/ \
        --baseline bench/baseline-metrics.json \
        --out persona-metrics.json
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bench_match import strict_match  # noqa: E402  # type: ignore[import-not-found]

REPO_ROOT = Path(__file__).resolve().parent.parent
PERSONAS_RS = REPO_ROOT / "tests" / "bench_personas.rs"

# Tolerance bands. Any single trigger fails the gate.
TOL_DEV_MRR_DROP = 0.05        # aggregate dev_mrr drop
TOL_AGG_H10_DROP = 0.02        # aggregate h10 drop (2pp)
TOL_PERSONA_H10_DROP = 0.05    # any persona h10 drop (5pp)
TOL_AGENT_MRR_DROP = 0.10      # aggregate agent_mrr (looser; ties grep)


@dataclass
class PQ:
    persona: str
    slug: str
    language: str
    query: str
    expected: list[str]
    mode: str  # "Agent" | "Developer"


def slug_for(name: str) -> str:
    """Filename-safe handle for a persona. Prefers the parenthetical token
    (e.g. 'rust-cli (ripgrep)' -> 'ripgrep') so DB filenames match the clone
    directories used by persona-bench.yml."""
    m = re.search(r"\(([^)]+)\)", name)
    if m:
        return m.group(1)
    return re.sub(r"[^A-Za-z0-9_.-]+", "-", name).strip("-")


def parse_personas() -> list[PQ]:
    text = PERSONAS_RS.read_text()
    persona_re = re.compile(
        r'name:\s*"([^"]+)",\s*'
        r'path:\s*"[^"]+",\s*'
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
        name, lang, body = pm.groups()
        slug = slug_for(name)
        for qm in pq_re.finditer(body):
            q, exp_raw, mode = qm.groups()
            expected = expected_re.findall(exp_raw)
            out.append(PQ(name, slug, lang, q, expected, mode))
    return out


def run_query(roux_bin: str, db_path: Path, query: str, top: int = 10) -> list[str]:
    """Return the ranked list of symbol names for a query."""
    proc = subprocess.run(
        [roux_bin, "query", query, "--db", str(db_path),
         "--format", "json", "--top", str(top)],
        capture_output=True, text=True,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"roux query failed for {db_path.name} / {query!r}: {proc.stderr.strip()}"
        )
    payload = json.loads(proc.stdout)
    # `symbols` preserves the PPR-ranked order (matches result.nodes in Rust).
    return [s["name"] for s in payload.get("symbols", [])]


def hit_at_k(results: list[tuple[list[str], list[str]]], k: int) -> float:
    if not results:
        return 0.0
    hits = 0
    for names, expected in results:
        if any(any(strict_match(n, exp) for exp in expected) for n in names[:k]):
            hits += 1
    return hits / len(results)


def mrr(results: list[tuple[list[str], list[str]]]) -> float:
    if not results:
        return 0.0
    total = 0.0
    for names, expected in results:
        for i, n in enumerate(names, start=1):
            if any(strict_match(n, exp) for exp in expected):
                total += 1.0 / i
                break
    return total / len(results)


def metrics_for(entries: list[tuple[list[str], list[str], str]]) -> dict:
    """Compute full metric block for a persona given (names, expected, mode) tuples."""
    all_r = [(n, e) for n, e, _ in entries]
    agent_r = [(n, e) for n, e, m in entries if m == "Agent"]
    dev_r = [(n, e) for n, e, m in entries if m == "Developer"]
    return {
        "h1": hit_at_k(all_r, 1),
        "h5": hit_at_k(all_r, 5),
        "h10": hit_at_k(all_r, 10),
        "mrr": mrr(all_r),
        "agent_h1": hit_at_k(agent_r, 1),
        "agent_mrr": mrr(agent_r),
        "dev_h1": hit_at_k(dev_r, 1),
        "dev_mrr": mrr(dev_r),
    }


def run_bench(roux_bin: str, db_dir: Path, queries: list[PQ]) -> dict:
    by_persona: dict[str, list[tuple[list[str], list[str], str]]] = {}
    persona_meta: dict[str, tuple[str, str]] = {}  # slug -> (display_name, language)

    for q in queries:
        persona_meta.setdefault(q.slug, (q.persona, q.language))
        db_path = db_dir / f"{q.slug}.sqlite"
        if not db_path.exists():
            print(f"  SKIP {q.slug}: no DB at {db_path}", file=sys.stderr)
            continue
        names = run_query(roux_bin, db_path, q.query)
        rank = next(
            (i + 1 for i, n in enumerate(names)
             if any(strict_match(n, e) for e in q.expected)),
            None,
        )
        status = "✓" if rank else "✗"
        rank_s = f"@{rank}" if rank else "miss"
        tag = "agent" if q.mode == "Agent" else "dev  "
        print(f"  {status} [{rank_s:>5}] ({tag}) {q.slug}: {q.query}", file=sys.stderr)
        by_persona.setdefault(q.slug, []).append((names, q.expected, q.mode))

    personas = []
    agg_keys = ("h1", "h5", "h10", "mrr",
                "agent_h1", "agent_mrr", "dev_h1", "dev_mrr")
    agg = {k: 0.0 for k in agg_keys}
    count = 0
    for slug, entries in by_persona.items():
        m = metrics_for(entries)
        display, lang = persona_meta[slug]
        personas.append({
            "name": display,
            "slug": slug,
            "language": lang,
            "metrics": m,
        })
        for k in agg:
            agg[k] += m[k]
        count += 1

    aggregate = None
    if count:
        aggregate = {"count": count, **{k: v / count for k, v in agg.items()}}

    return {
        "timestamp": int(time.time()),
        "commit": os.environ.get("GITHUB_SHA"),
        "personas": personas,
        "aggregate": aggregate,
    }


# ─── Diff ───────────────────────────────────────────────────────────────

def diff_vs_baseline(current: dict, baseline: dict) -> list[str]:
    """Return list of regression strings (empty = pass)."""
    fails: list[str] = []
    cur_agg = current.get("aggregate") or {}
    base_agg = baseline.get("aggregate") or {}

    def drop(metric: str) -> float:
        return base_agg.get(metric, 0.0) - cur_agg.get(metric, 0.0)

    if drop("dev_mrr") > TOL_DEV_MRR_DROP:
        fails.append(
            f"aggregate dev_mrr regressed by {drop('dev_mrr'):.3f} "
            f"(>{TOL_DEV_MRR_DROP}): {base_agg.get('dev_mrr', 0):.3f} → "
            f"{cur_agg.get('dev_mrr', 0):.3f}"
        )
    if drop("h10") > TOL_AGG_H10_DROP:
        fails.append(
            f"aggregate h10 regressed by {drop('h10') * 100:.1f}pp "
            f"(>{TOL_AGG_H10_DROP * 100:.0f}pp): {base_agg.get('h10', 0) * 100:.1f}% → "
            f"{cur_agg.get('h10', 0) * 100:.1f}%"
        )
    if drop("agent_mrr") > TOL_AGENT_MRR_DROP:
        fails.append(
            f"aggregate agent_mrr regressed by {drop('agent_mrr'):.3f} "
            f"(>{TOL_AGENT_MRR_DROP}): {base_agg.get('agent_mrr', 0):.3f} → "
            f"{cur_agg.get('agent_mrr', 0):.3f}"
        )

    base_personas = {p["slug"]: p for p in baseline.get("personas", []) if "slug" in p}
    # Older baselines may lack `slug`; fall back to display name.
    if not base_personas:
        base_personas = {p["name"]: p for p in baseline.get("personas", [])}
    for p in current.get("personas", []):
        key = p["slug"] if p["slug"] in base_personas else p["name"]
        bp = base_personas.get(key)
        if not bp:
            continue
        cur_h10 = p["metrics"].get("h10", 0.0)
        base_h10 = bp.get("metrics", {}).get("h10", 0.0)
        if base_h10 - cur_h10 > TOL_PERSONA_H10_DROP:
            fails.append(
                f"persona {p['slug']} h10 regressed by "
                f"{(base_h10 - cur_h10) * 100:.1f}pp (>{TOL_PERSONA_H10_DROP * 100:.0f}pp): "
                f"{base_h10 * 100:.1f}% → {cur_h10 * 100:.1f}%"
            )
    return fails


# ─── Markdown rendering ─────────────────────────────────────────────────

def render_markdown(current: dict, baseline: dict | None, fails: list[str]) -> str:
    lines: list[str] = []
    title = "## Persona regression gate"
    if fails:
        title += " — ❌ regression"
    elif baseline:
        title += " — ✅ within tolerance"
    else:
        title += " — ℹ️ no baseline"
    lines.append(title)
    lines.append("")

    cur_agg = current.get("aggregate") or {}
    base_agg = (baseline or {}).get("aggregate") or {}

    def cell(metric: str, fmt: str = "{:.3f}") -> str:
        cur = cur_agg.get(metric)
        if cur is None:
            return "—"
        s = fmt.format(cur)
        if metric in base_agg:
            delta = cur - base_agg[metric]
            arrow = "→" if abs(delta) < 1e-9 else ("▲" if delta > 0 else "▼")
            s += f" {arrow}{fmt.format(abs(delta))}"
        return s

    lines.append("### Aggregate")
    lines.append("| metric | value (Δ vs baseline) |")
    lines.append("|---|---|")
    lines.append(f"| h1 | {cell('h1')} |")
    lines.append(f"| h10 | {cell('h10')} |")
    lines.append(f"| mrr | {cell('mrr')} |")
    lines.append(f"| agent_mrr | {cell('agent_mrr')} |")
    lines.append(f"| dev_mrr | {cell('dev_mrr')} |")
    lines.append("")

    lines.append("### Per-persona (Hit@10 / MRR)")
    lines.append("| persona | h10 | mrr | dev_mrr | agent_mrr |")
    lines.append("|---|---|---|---|---|")
    base_personas = {p.get("slug") or p.get("name"): p
                     for p in (baseline or {}).get("personas", [])}
    for p in current.get("personas", []):
        m = p["metrics"]
        bp = (base_personas.get(p.get("slug")) or
              base_personas.get(p.get("name")) or {})
        bm = bp.get("metrics", {}) if bp else {}

        def pcell(k: str, fmt: str = "{:.3f}") -> str:
            cur = m.get(k, 0.0)
            s = fmt.format(cur)
            if k in bm:
                d = cur - bm[k]
                arrow = "→" if abs(d) < 1e-9 else ("▲" if d > 0 else "▼")
                s += f" {arrow}{fmt.format(abs(d))}"
            return s

        lines.append(
            f"| {p['slug']} | {pcell('h10')} | {pcell('mrr')} | "
            f"{pcell('dev_mrr')} | {pcell('agent_mrr')} |"
        )

    if fails:
        lines.append("")
        lines.append("### Regressions")
        for f in fails:
            lines.append(f"- {f}")
    return "\n".join(lines) + "\n"


# ─── Main ───────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db-dir", required=True, type=Path,
                    help="Directory containing <slug>.sqlite snapshot DBs")
    ap.add_argument("--baseline", type=Path,
                    help="Path to baseline-metrics.json (optional; no diff if absent)")
    ap.add_argument("--out", type=Path,
                    help="Write current metrics JSON to this path")
    ap.add_argument("--md-out", type=Path,
                    help="Write markdown summary (for PR comment / step summary)")
    ap.add_argument("--roux-bin", default=os.environ.get("ROUX_BIN", "roux"),
                    help="Path to roux binary (default: $ROUX_BIN or 'roux')")
    args = ap.parse_args()

    if not args.db_dir.is_dir():
        print(f"db-dir {args.db_dir} not found", file=sys.stderr)
        return 2

    queries = parse_personas()
    if not queries:
        print("no persona queries parsed", file=sys.stderr)
        return 2

    current = run_bench(args.roux_bin, args.db_dir, queries)

    if args.out:
        args.out.write_text(json.dumps(current, indent=2))

    baseline = None
    fails: list[str] = []
    if args.baseline and args.baseline.exists():
        baseline = json.loads(args.baseline.read_text())
        fails = diff_vs_baseline(current, baseline)

    md = render_markdown(current, baseline, fails)
    print(md)
    if args.md_out:
        args.md_out.write_text(md)

    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
