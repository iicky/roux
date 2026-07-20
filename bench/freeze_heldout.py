#!/usr/bin/env python3
"""Freeze a harvested DRAFT held-out set into a human-reviewed frozen set.

This is the gold-tightening / review pass. It:
  * assigns the final S/L bucket by DISTINCTIVE query<->gold lexical overlap (exact token
    match, source-prefix stripped, stoplist-filtered). Reproduces the hand-frozen ripgrep
    set's 24/24 bucket labels (validated).
      L = distinctive lexical overlap (control; BM25 can seed it)
      S = no distinctive overlap (pure vocab/semantic gap — the anti-Goodhart test)
  * PRUNES maintainer-process queries that are not user retrieval questions (CI/TST/STYLE/
    doctest/lint/typo/bare-path titles) and curated weak-gold mis-pins.
  * drops any residual generic gold (dunders) and queries left empty.
  * records a full audit trail (freeze_provenance.review.dropped, with reason codes) so the
    human-reviewed upgrade is reproducible from the draft.

Generic-constructor gold (dunders, from_low_args, per-persona god-symbols) is already dropped
at pin time by the harvester (skip_syms / GENERIC_SYM); this is the belt-and-suspenders pass.
Refuses to overwrite ripgrep's hand-frozen set (no auto_frozen flag) unless --force.

Usage:
  python3 bench/freeze_heldout.py                 # freeze every gold_*.draft.json
  python3 bench/freeze_heldout.py pandas gin      # subset by persona
  python3 bench/freeze_heldout.py --force         # allow overwriting an existing frozen set
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from datetime import date
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BENCH = ROOT / "bench"

# English + bench-domain tokens that are NOT distinctive controls. The maintainer flagged
# stopword overlap ("line"/"match"/"with") as spurious — those are the hard S cases.
STOP = set("""a an the and or of to in on for with without at by from into is are be
was were do does did not no yes it its this that these those as if then than when
where which who whom what how why can could should would may might must will shall
i we you he she they them his her their our your my me us via per only just also so
""".split()) | {
    "line", "match", "matches", "matching", "with", "file", "files", "error", "errors",
    "bug", "issue", "use", "using", "used", "add", "adding", "added", "get", "set", "new",
    "run", "running", "support", "fix", "fixed", "doesnt", "dont", "cant", "wont",
    "behavior", "behaviour", "value", "values", "option", "options", "flag", "flags",
    "string", "output", "input", "data",
}

# Maintainer-process titles that are not user "where is X" retrieval questions.
OFF_TOPIC = re.compile(
    r"^\s*(?:ci\b|tst:|style\b|test:|cln:?)"                 # process prefixes (CLN = cleanup)
    r"|ci failing|doctest|pylint|flake8|\bmypy\b|redefined-outer-name"
    r"|fix docstring quotes|standardize use of"              # sweeping doc edits
    r"|\btypo\b",
    re.I,
)
BARE_PATH = re.compile(r"^[\w./\\-]+\.(?:py|rs|go|ts|tsx|js|cpp|cc|cxx|h|hpp|hh|ino)\s*$", re.I)
GENERIC_SYM = re.compile(r"^__\w+__$")       # dunders
MEGA_SYM = frozenset({"from_low_args"})       # known cross-cutting god-constructors
SINGLE_CHAR = re.compile(r"^_?[A-Za-z]$")     # gold like `f`/`x`: un-retrievable junk


def is_junk_gold(qn: str) -> bool:
    s = qn.split("::")[-1]
    return bool(GENERIC_SYM.match(s)) or s in MEGA_SYM or bool(SINGLE_CHAR.match(s))

# Curated weak-gold: (persona, issue) whose pinned gold is clearly not the answer.
WEAK_GOLD = {
    ("gin", 20),       # "Support for 405 Method Not Allowed" -> gin::Value (generic tree node)
    ("pandas", 34593), # "Can't figure out how to get exponential working" -> Window (vague, generic)
    ("pandas", 35491), # "Add note to DataFrame.compare" -> _construct_result (unrelated internal)
}

# Balance/reviewability cap: retain the N most-recent (highest issue#) queries per persona;
# the rest stay in the draft as backlog. pandas' deep history yields ~194 raw — cap to a
# reviewed, balanced slice.
RETAIN_RECENT = {"pandas": 100}


def toks(s: str) -> set[str]:
    out: set[str] = set()
    for part in re.split(r"[^A-Za-z0-9]+", s):
        for w in re.findall(r"[A-Z]?[a-z0-9]+|[A-Z]+(?![a-z])", part):
            if len(w) >= 3:
                out.add(w.lower())
    return out


def gold_toks(gold: list[str]) -> set[str]:
    out: set[str] = set()
    for g in gold:
        short = g.split("::", 1)[1] if "::" in g else g
        out |= toks(short)
    return out


def bucket_of(query: str, gold: list[str]) -> tuple[str, list[str]]:
    overlap = (toks(query) & gold_toks(gold)) - STOP
    return ("L" if overlap else "S"), sorted(overlap)


def roux_commit() -> str:
    return subprocess.run(
        ["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
        capture_output=True, text=True,
    ).stdout.strip() or "?"


def drop_reason(q: dict, persona: str) -> str | None:
    if OFF_TOPIC.search(q["query"]) or BARE_PATH.match(q["query"].strip()):
        return "off_topic"
    if (persona, q["provenance"]["issue"]) in WEAK_GOLD:
        return "weak_gold"
    if not [g for g in q["gold"] if not is_junk_gold(g)]:
        return "empty_gold"
    return None


def freeze(draft_path: Path, force: bool, named: set[str]) -> dict | None:
    draft = json.loads(draft_path.read_text())
    persona = draft["source_name"]
    out_path = BENCH / f"gold_{persona}.frozen.json"
    explicit = persona in named   # exact source_name match, not substring
    if out_path.exists():
        existing = json.loads(out_path.read_text())
        human = not existing.get("freeze_provenance", {}).get("auto_frozen")
        # human-reviewed sets are only overwritten when named explicitly AND forced —
        # a blanket `--force` must never clobber curated gold (e.g. ripgrep).
        if human and not (force and explicit):
            print(f"— skip {persona}: {out_path.name} is human-reviewed "
                  f"(name it explicitly with --force to overwrite)")
            return None
        if not human and not force:
            print(f"— skip {persona}: {out_path.name} already frozen (use --force)")
            return None

    raw = draft["queries"]
    cap = RETAIN_RECENT.get(persona)
    backlog = 0
    if cap and len(raw) > cap:
        raw = sorted(raw, key=lambda q: q["provenance"]["issue"], reverse=True)[:cap]
        backlog = len(draft["queries"]) - cap
    queries, dropped = [], []
    for q in raw:
        reason = drop_reason(q, persona)
        if reason:
            dropped.append({"id": q["id"], "reason": reason, "query": q["query"][:80]})
            continue
        gold = [g for g in q["gold"] if not is_junk_gold(g)]
        b, overlap = bucket_of(q["query"], gold)
        queries.append({
            "id": q["id"], "bucket": b, "query": q["query"], "gold": gold,
            "lexical_overlap": overlap, "roux_rank_at_freeze": q.get("roux_rank"),
            "provenance": q["provenance"],
        })
    queries.sort(key=lambda x: x["provenance"]["issue"])
    dist = {k: sum(1 for x in queries if x["bucket"] == k) for k in ("S", "L")}
    reasons = {r: sum(1 for d in dropped if d["reason"] == r) for r in {d["reason"] for d in dropped}}
    doc = {
        "_comment": (
            f"HUMAN-REVIEWED frozen held-out set ({persona}) — anti-Goodhart. "
            "Harvested by bench/harvest_persona_queries.py, frozen/reviewed by bench/freeze_heldout.py. "
            "S/L by distinctive query<->gold(qualified_name) token overlap (source-prefix stripped, "
            "stoplist-filtered): S = no distinctive overlap (vocab/semantic gap — the real test); "
            "L = distinctive overlap (control). gold = qualified_name substrings; any in top-K = hit. "
            "Reviewed: generic-constructor gold removed (harvester + freeze), maintainer-process queries "
            "and weak-gold mis-pins pruned (see freeze_provenance.review). FROZEN: never tune against it; "
            "report SEPARATELY from the CI gate."
        ),
        "source_name": persona,
        "index_path": draft["index_path"],
        "pinned_ref": draft["pinned_ref"],
        "freeze_provenance": {
            "frozen": date.today().isoformat(),
            "roux_commit": roux_commit(),
            "auto_frozen": False,
            "human_reviewed": True,
            "from_draft": f"bench/{draft_path.name}",
            "harvester": "bench/harvest_persona_queries.py",
            "triage": "S/L by distinctive query<->gold token overlap (source-prefix stripped, stoplist-filtered)",
            "review": {
                "reviewer": "gold-tightening pass",
                "generic_gold": "dunders/from_low_args/per-persona skip_syms dropped at pin time + freeze safety-net",
                "off_topic_rule": "CI/TST/STYLE/test/CLN prefixes, doctest/pylint/flake8/mypy, typo, sweeping-doc edits, bare-path titles",
                "weak_gold_manual": sorted(f"{p}-{i}" for p, i in WEAK_GOLD if p == persona),
                "retain_recent_cap": cap,
                "backlog_in_draft": backlog,
                "dropped_counts": reasons,
                "dropped": dropped,
            },
        },
        "distribution": dist,
        "queries": queries,
    }
    out_path.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"[{persona}] {len(queries)} kept, {len(dropped)} dropped {reasons or ''} -> {out_path.name}  buckets: {dist}")
    return {"persona": persona, "kept": len(queries), "dist": dist}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("personas", nargs="*", help="filter by persona (default: all drafts)")
    ap.add_argument("--force", action="store_true", help="overwrite an existing frozen set")
    args = ap.parse_args()

    drafts = sorted(BENCH.glob("gold_*.draft.json"))
    if args.personas:
        drafts = [d for d in drafts if any(p.lower() in d.stem.lower() for p in args.personas)]
    if not drafts:
        sys.exit("no draft sets found (bench/gold_*.draft.json)")
    results = [r for r in (freeze(d, args.force, set(args.personas)) for d in drafts) if r]
    total = sum(r["kept"] for r in results)
    combined = {"S": sum(r["dist"]["S"] for r in results), "L": sum(r["dist"]["L"] for r in results)}
    print(f"\nfroze {len(results)} sets, combined kept N={total}  buckets={combined}")


if __name__ == "__main__":
    main()
