#!/usr/bin/env python3
"""Held-out token economics — the anti-Goodhart companion to token_economics.py.

token_economics.py runs its agent arms on the persona GATE (lexically friendly,
post-hoc edited). This runs the SAME drivers and the SAME success grader on the
BLIND frozen held-out queries (bench/gold_*.frozen.json — real closed-issue
titles, never tuned against). It answers the honest question: does roux-as-
preprocessor's token/latency win survive queries the tool has never seen?

Two arms (both from token_economics.py, verbatim):
  no-roux       — agent with Read/Grep/Glob/Bash, no roux.
  context-prep  — roux runs ONCE up front (`roux query --format skeleton`), the
                  ranked block is injected into the prompt prefix, no live tool.

Each persona's frozen snapshot is copied to <clone>/.roux/db.sqlite so the
context arm's `roux query --local` hits the exact human-reviewed index the
held-out retrieval numbers came from. Clones are grep_floor_eval.CLONES (pinned
to the snapshot refs).

Env:
  ONLY_AGENTS=claude   ONLY_PERSONAS=ripgrep,pandas   ONLY_QUERIES=8 (per persona)
  RUNS_PER_TASK=1      ARMS=no-roux,context-prep
Needs the agent CLI (claude/codex/vibe) on PATH with working auth.
"""
from __future__ import annotations

import json
import os
import shutil
import sys
import time
from dataclasses import asdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"
SNAP = BENCH / "snapshots"
sys.path.insert(0, str(BENCH))
from token_economics import (  # noqa: E402
    PQ, DRIVERS, RESULTS_DIR, cli_available, short_persona,
)
from grep_floor_eval import CLONES  # noqa: E402

LANG = {"ripgrep": "rust", "pandas": "python", "gin": "go",
        "remix": "typescript", "Marlin": "cpp"}


def _expected(q: dict) -> list[str]:
    """Leaf symbol names an agent would actually name (gold is qualified)."""
    out = {g.split("::")[-1] for g in q["gold"]}
    out.update(q.get("provenance", {}).get("gold_candidates") or [])
    return sorted(n for n in out if n)


def load_heldout(only_personas: set[str], cap: int) -> list[PQ]:
    pqs: list[PQ] = []
    for gp in sorted(BENCH.glob("gold_*.frozen.json")):
        spec = json.load(open(gp))
        name = spec["source_name"]
        if only_personas and name not in only_personas:
            continue
        clone = CLONES.get(name)
        snap = SNAP / f"{name}.sqlite"
        if not clone or not Path(clone).exists():
            print(f"[skip] {name}: clone {clone} missing", file=sys.stderr)
            continue
        if not snap.exists():
            print(f"[skip] {name}: snapshot missing", file=sys.stderr)
            continue
        dst = Path(clone) / ".roux" / "db.sqlite"
        dst.parent.mkdir(parents=True, exist_ok=True)
        if not dst.exists() or dst.stat().st_size != snap.stat().st_size:
            shutil.copy(snap, dst)
        qs = spec["queries"]
        if cap:
            qs = qs[:cap]
        for q in qs:
            pqs.append(PQ(persona=name, path=clone, language=LANG.get(name, "?"),
                          query=q["query"], expected=_expected(q), mode="Agent"))
    return pqs


def main() -> int:
    only_agents = set(filter(None, os.environ.get("ONLY_AGENTS", "claude").split(",")))
    only_personas = set(filter(None, os.environ.get("ONLY_PERSONAS", "").split(",")))
    # Safe by default: cap per-persona queries unless ONLY_QUERIES or RUN_FULL=1.
    cap = int(os.environ.get("ONLY_QUERIES", "0") or 0)
    if cap == 0 and os.environ.get("RUN_FULL") != "1":
        cap = 3
    runs = int(os.environ.get("RUNS_PER_TASK", "1"))
    arms = tuple(filter(None, os.environ.get("ARMS", "no-roux,context-prep").split(",")))

    pqs = load_heldout(only_personas, cap)
    if not pqs:
        print("no held-out queries after filtering", file=sys.stderr)
        return 1
    agents = [a for a in sorted(only_agents) if cli_available(a)]
    if not agents:
        print("no agents on PATH", file=sys.stderr)
        return 1

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    ts = time.strftime("%Y%m%dT%H%M")
    out = RESULTS_DIR / f"heldout_token_economics_{ts}.jsonl"
    total = len(pqs) * len(agents) * len(arms) * runs
    done = 0
    print(f"[heldout-te] {len(pqs)} queries × {len(agents)} agents × "
          f"{len(arms)} arms ({','.join(arms)}) × {runs} = {total} runs → {out}",
          file=sys.stderr)
    with out.open("w") as fh:
        for pq in pqs:
            for agent in agents:
                drive, model_fn = DRIVERS[agent]
                model = model_fn()
                for arm in arms:
                    for run_idx in range(1, runs + 1):
                        done += 1
                        print(f"[{done}/{total}] {agent} {arm} "
                              f"{short_persona(pq.persona)}/{pq.query[:40]}... run={run_idx}",
                              file=sys.stderr)
                        result = drive(pq, arm, run_idx, model)
                        fh.write(json.dumps(asdict(result)) + "\n")
                        fh.flush()
    print(f"[heldout-te] done → {out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
