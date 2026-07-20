#!/usr/bin/env python3
"""Out-of-distribution external check: roux vs a coverage-grep FLOOR at FILE
granularity, on frozen polars fix PRs (bench/external_polars.frozen.json).

Why this exists: every other set here is one of five frozen personas graded on
SYMBOLS. This is the honest counter-check — a repo roux has nothing to do with,
graded on FILES (does a changed source file land in the top-k?), which is the
axis where lexical retrieval is strongest. It is where roux LOSES: whole-file
token coverage beats symbol-ranked retrieval at "which file". Reported so the
README can state that plainly.

Query variants (both tools get the identical input):
  - title : PR title minus the conventional-commit prefix (code-jargon)
  - nl    : linked-issue title+body, capped (natural-language bug report)
Grep floor = whole-identifier token coverage over each file, ranked by
(distinct query tokens present, total occurrences) — the polars-style floor,
NOT roux's single-token grep floor.

Setup (network; the corpus is a pinned clone, like the persona snapshots):
  git clone https://github.com/pola-rs/polars /tmp/polars-bench
  git -C /tmp/polars-bench checkout <pinned_ref from the fixture>
  (cd /tmp/polars-bench && "$OLDPWD"/target/release/roux add crates --local --name polars)
  python3 bench/external_polars_eval.py            # POLARS_DIR=/tmp/polars-bench

  python3 bench/external_polars_eval.py --refresh  # re-harvest PRs via `gh` (drifts)
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUX = Path(os.environ.get("ROUX_BIN", ROOT / "target" / "release" / "roux"))
FIXTURE = ROOT / "bench" / "external_polars.frozen.json"
POLARS = Path(os.environ.get("POLARS_DIR", "/tmp/polars-bench"))
TOKEN = re.compile(r"[A-Za-z_][A-Za-z0-9_]+")
CAP = 8000
STOP = set("fix fixes bug regression error the a an of to in for and or with when if is are be add "
           "remove update use using into from on at panic issue make do not no also should correct "
           "incorrect wrong handle allow avoid case cases return returns this that it as reproduce "
           "reproducible example version expected actual output following code python rust import "
           "print dataframe polars would should have has was were will can could".split())


def build_corpus(crates: Path) -> dict[str, Counter]:
    files = {}
    for p in crates.rglob("*.rs"):
        try:
            files[str(p.relative_to(crates))] = Counter(
                t.lower() for t in TOKEN.findall(p.read_text(errors="ignore")))
        except OSError:
            pass
    return files


def query_text(title: str) -> str:
    t = re.sub(r"^\s*fix(\([^)]*\))?\s*:\s*", "", title, flags=re.I)
    return re.sub(r"^\s*fix\s+", "", t, flags=re.I).strip()


def content_tokens(text: str) -> list[str]:
    return [w for w in (m.lower() for m in TOKEN.findall(text)) if w not in STOP and len(w) >= 3]


def code_tokens(text: str) -> list[str]:
    toks = set()
    for m in re.findall(r"`([^`]+)`", text):
        toks.update(w.lower() for w in TOKEN.findall(m))
    toks.update(w.lower() for w in re.findall(r"\b[a-zA-Z][a-zA-Z0-9]*_[a-zA-Z0-9_]+\b", text))
    toks.update(w.lower() for w in re.findall(r"\b[A-Z][a-z]+[A-Z][A-Za-z0-9]*\b", text))
    return [t for t in toks if t not in STOP and len(t) >= 3]


def grep_topk(files: dict[str, Counter], toks: list[str], k: int) -> list[str]:
    toks = set(toks)
    if not toks:
        return []
    scored = []
    for rel, cnt in files.items():
        distinct = total = 0
        for w in toks:
            c = cnt.get(w, 0)
            if c:
                distinct += 1
                total += c
        if distinct:
            scored.append((-distinct, -total, rel))
    scored.sort()
    return [rel for *_, rel in scored[:k]]


def roux_topk(query: str, k: int) -> list[str]:
    if not query.strip():
        return []
    proc = subprocess.run([str(ROUX), "query", query, "--local", "--format", "json", "--top", "60"],
                          cwd=str(POLARS), capture_output=True, text=True, check=False)
    try:
        data = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return []
    seen: list[str] = []
    for s in data.get("symbols", []):
        f = s.get("file")
        if f and f not in seen:
            seen.append(f)
    return seen[:k]


def pct(hits: list[bool]) -> str:
    n = len(hits)
    return f"{sum(hits) / n * 100:5.1f}% ({sum(hits)}/{n})" if n else "n/a"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--refresh", action="store_true", help="re-harvest PRs via gh (network; drifts)")
    args = ap.parse_args()
    if args.refresh:
        print("refresh: re-harvest with `gh pr list --repo pola-rs/polars --search 'fix in:title' "
              "--json number,title,body,files,closingIssuesReferences` then rebuild the fixture.",
              file=sys.stderr)
        return

    fixture = json.loads(FIXTURE.read_text())
    crates = POLARS / "crates"
    if not crates.exists() or not (POLARS / ".roux").exists():
        print(f"POLARS_DIR={POLARS} needs a clone at {fixture['pinned_ref'][:10]} + a roux index. "
              "See the module docstring.", file=sys.stderr)
        sys.exit(2)
    files = build_corpus(crates)
    print(f"corpus: {len(files)} .rs files  |  pinned {fixture['pinned_ref'][:10]}")

    title_rx, title_gp = [], []
    nl_rx, nl_gc = [], []
    for q in fixture["queries"]:
        gt = {f for f in q["changed_rs_crates"] if f in files}
        if not gt:
            continue
        qt = query_text(q["title"])
        title_rx.append(any(f in gt for f in roux_topk(qt, 10)))
        title_gp.append(any(f in gt for f in grep_topk(files, content_tokens(qt), 10)))
        if q["linked_issue_texts"]:
            nlq = "\n".join(q["linked_issue_texts"])[:CAP]
            nl_rx.append(any(f in gt for f in roux_topk(nlq, 10)))
            nl_gc.append(any(f in gt for f in grep_topk(files, code_tokens(nlq), 10)))

    print("\nFILE-level Hit@10 (a changed .rs in the top-10 unique files):")
    print(f"  TITLE query (code-jargon)   roux {pct(title_rx)}   grep-coverage {pct(title_gp)}")
    print(f"  NL bug-report query         roux {pct(nl_rx)}   grep-code {pct(nl_gc)}")
    print("\nExpected: grep-coverage wins on file-finding — roux's edge is symbol ranking "
          "(see coverage_floor_eval.py / heldout_eval.py), not this axis.")


if __name__ == "__main__":
    main()
