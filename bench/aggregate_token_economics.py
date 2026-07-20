#!/usr/bin/env python3
"""Roll up token-economics JSONL into cost + arm-delta tables.

Reads one or more `bench/results/token_economics_*.jsonl` files, applies
per-model pricing, and emits a markdown summary that compares the no-roux
and with-roux arms per (agent, model, persona).

Pricing tables are public list prices as of the run date — they are NOT
the source of truth for billing, but they are stable enough to compare
arms against each other. Override via env vars or by editing PRICING.

Usage:
    python bench/aggregate_token_economics.py bench/results/*.jsonl
    python bench/aggregate_token_economics.py --by query bench/results/*.jsonl

Exit 0 always; this is a reporter, not a gate.
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path
from statistics import mean

# ─── Pricing ────────────────────────────────────────────────────────────
# USD per 1M tokens. Sourced from public docs (anthropic.com/pricing,
# openai.com/api/pricing, mistral.ai/pricing). Cache-read pricing assumes
# the standard prompt-cache discount; cache-creation pricing assumes the
# 5-minute cache write cost where the model exposes one separately.

PRICING: dict[str, dict[str, float]] = {
    # Anthropic
    "claude-sonnet-4-6":   {"in": 3.00,  "out": 15.00, "cache_read": 0.30,  "cache_create": 3.75},
    "claude-opus-4-7":     {"in": 15.00, "out": 75.00, "cache_read": 1.50,  "cache_create": 18.75},
    "claude-haiku-4-5":    {"in": 1.00,  "out": 5.00,  "cache_read": 0.10,  "cache_create": 1.25},
    # OpenAI / Codex
    "gpt-5-codex":         {"in": 1.25,  "out": 10.00, "cache_read": 0.125, "cache_create": 0.0},
    "":                    {"in": 1.25,  "out": 10.00, "cache_read": 0.125, "cache_create": 0.0},  # codex default
    # Mistral / Vibe (Devstral 2 list price)
    "devstral-2":          {"in": 0.20,  "out": 0.60,  "cache_read": 0.0,   "cache_create": 0.0},
}

# Keys that exist in newer schema but not older
TOKEN_KEYS = (
    "input_tokens", "output_tokens",
    "cache_read_tokens", "cache_creation_tokens",
    "reasoning_output_tokens",
)


def cost_usd(row: dict) -> float:
    pricing = PRICING.get(row.get("model") or "")
    if not pricing:
        return 0.0
    inp = row.get("input_tokens", 0) or 0
    out = row.get("output_tokens", 0) or 0
    cr = row.get("cache_read_tokens", 0) or 0
    cc = row.get("cache_creation_tokens", 0) or 0
    reasoning = row.get("reasoning_output_tokens", 0) or 0
    return (
        inp * pricing["in"]
        + out * pricing["out"]
        + cr * pricing["cache_read"]
        + cc * pricing["cache_create"]
        + reasoning * pricing["out"]  # reasoning billed at output rate
    ) / 1_000_000


def total_input_with_cache(row: dict) -> int:
    """Total input cost in 'effective' tokens at full rate.

    Useful for comparing arms without the cache discount muddying things —
    counts cache-reads at face value (not their discounted price)."""
    return sum(int(row.get(k, 0) or 0) for k in (
        "input_tokens", "cache_read_tokens", "cache_creation_tokens",
    ))


# ─── Loading ────────────────────────────────────────────────────────────

def load_rows(paths: list[Path]) -> list[dict]:
    rows: list[dict] = []
    for p in paths:
        with p.open() as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    rows.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    return rows


# ─── Aggregation ────────────────────────────────────────────────────────

def aggregate(rows: list[dict], group_by: tuple[str, ...]) -> list[dict]:
    """Group rows and compute per-group averages."""
    buckets: dict[tuple, list[dict]] = defaultdict(list)
    for r in rows:
        if r.get("error"):
            continue  # exclude failed/timed-out runs from cost averages
        key = tuple(r.get(k, "") for k in group_by)
        buckets[key].append(r)

    out: list[dict] = []
    for key, bucket in sorted(buckets.items()):
        if not bucket:
            continue
        d = dict(zip(group_by, key))
        d["n"] = len(bucket)
        d["success_rate"] = sum(1 for b in bucket if b.get("success")) / len(bucket)
        d["wall_ms"] = mean(b.get("wall_ms", 0) for b in bucket)
        d["num_turns"] = mean(b.get("num_turns", 0) or 0 for b in bucket)
        for k in TOKEN_KEYS:
            d[k] = mean(b.get(k, 0) or 0 for b in bucket)
        d["effective_input"] = mean(total_input_with_cache(b) for b in bucket)
        d["cost_usd"] = mean(cost_usd(b) for b in bucket)
        out.append(d)
    return out


def arm_deltas(per_arm: list[dict], pivot_keys: tuple[str, ...]) -> list[dict]:
    """Pair (arm=no-roux, arm=with-roux) rows and compute deltas."""
    by_pivot: dict[tuple, dict[str, dict]] = defaultdict(dict)
    for row in per_arm:
        pivot = tuple(row.get(k, "") for k in pivot_keys)
        by_pivot[pivot][row.get("arm", "")] = row

    out: list[dict] = []
    for pivot, arms in sorted(by_pivot.items()):
        no_r = arms.get("no-roux")
        with_r = arms.get("with-roux")
        if not no_r or not with_r:
            continue
        rec = dict(zip(pivot_keys, pivot))
        for k in ("cost_usd", "wall_ms", "num_turns",
                  "input_tokens", "output_tokens",
                  "cache_read_tokens", "cache_creation_tokens",
                  "effective_input"):
            n = no_r.get(k, 0) or 0
            w = with_r.get(k, 0) or 0
            rec[f"{k}_noroux"] = n
            rec[f"{k}_withroux"] = w
            rec[f"{k}_delta"] = w - n
            rec[f"{k}_pct"] = ((w - n) / n * 100.0) if n else 0.0
        rec["success_noroux"] = no_r.get("success_rate", 0.0)
        rec["success_withroux"] = with_r.get("success_rate", 0.0)
        rec["n"] = min(no_r.get("n", 0), with_r.get("n", 0))
        out.append(rec)
    return out


# ─── Rendering ──────────────────────────────────────────────────────────

def render(deltas: list[dict], pivot_keys: tuple[str, ...]) -> str:
    if not deltas:
        return "_(no paired arms found — both no-roux and with-roux must be present per pivot)_\n"

    lines: list[str] = []
    lines.append("# Token economics — arm comparison")
    lines.append("")
    lines.append(f"_n runs per arm = min across pivot; pivot = {' × '.join(pivot_keys)}_")
    lines.append("")
    headers = list(pivot_keys) + [
        "n", "success no→with",
        "cost no→with (Δ%)",
        "wall_ms no→with (Δ%)",
        "turns no→with",
        "in_tokens no→with (Δ%)",
        "cache_read no→with",
    ]
    lines.append("| " + " | ".join(headers) + " |")
    lines.append("|" + "|".join(["---"] * len(headers)) + "|")
    for d in deltas:
        cells = [str(d.get(k, "")) for k in pivot_keys]
        cells.append(str(d.get("n", 0)))
        cells.append(f"{d['success_noroux']:.0%} → {d['success_withroux']:.0%}")
        cells.append(
            f"${d['cost_usd_noroux']:.4f} → ${d['cost_usd_withroux']:.4f} "
            f"({d['cost_usd_pct']:+.0f}%)"
        )
        cells.append(
            f"{d['wall_ms_noroux']:.0f} → {d['wall_ms_withroux']:.0f} "
            f"({d['wall_ms_pct']:+.0f}%)"
        )
        cells.append(f"{d['num_turns_noroux']:.1f} → {d['num_turns_withroux']:.1f}")
        cells.append(
            f"{d['input_tokens_noroux']:.0f} → {d['input_tokens_withroux']:.0f} "
            f"({d['input_tokens_pct']:+.0f}%)"
        )
        cells.append(
            f"{d['cache_read_tokens_noroux']:.0f} → {d['cache_read_tokens_withroux']:.0f}"
        )
        lines.append("| " + " | ".join(cells) + " |")
    lines.append("")
    return "\n".join(lines) + "\n"


# ─── Main ───────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("paths", nargs="+", type=Path,
                    help="JSONL result files (e.g. bench/results/*.jsonl)")
    ap.add_argument("--by", choices=("agent", "persona", "query"), default="agent",
                    help="Pivot the arm comparison: per-agent (default), "
                         "per-persona, or per-query.")
    ap.add_argument("--out", type=Path, help="Write markdown to this file")
    args = ap.parse_args()

    rows = load_rows(args.paths)
    if not rows:
        print("no rows loaded")
        return 0

    if args.by == "agent":
        pivot_keys = ("agent", "model")
    elif args.by == "persona":
        pivot_keys = ("agent", "model", "persona")
    else:
        pivot_keys = ("agent", "model", "persona", "query")

    per_arm = aggregate(rows, pivot_keys + ("arm",))
    deltas = arm_deltas(per_arm, pivot_keys)

    md = render(deltas, pivot_keys)
    print(md)
    if args.out:
        args.out.write_text(md)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
