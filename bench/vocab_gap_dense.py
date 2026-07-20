#!/usr/bin/env python3
"""Offline dense retrieval arm for the S (vocab-gap) diagnosis — CEILING ONLY.

This is a MEASUREMENT reference arm, not a shipped feature. roux ships no
embeddings (CPU-only, offline is the product promise). This script asks a
narrow question: if we replaced roux's BM25+PPR ranking with a strong general
sentence-embedding model, would it recover the frozen S-bucket queries that
lexical+graph retrieval floors on? If a strong dense model ALSO floors on the
same queries, that is evidence (under this model, not a proof) that the residual
gap is not recoverable by embeddings either — largely uninformative-query-title
noise — and the no-embeddings architecture is near its achievable ceiling.

Method: for each frozen S query, embed the query and rank it (cosine) against
the dense embeddings of every code symbol in the frozen snapshot (file and
doc_section structural nodes excluded — they are never gold, a best case for
dense). A gold hit in the
top-K counts, using the same strict_match predicate as heldout_eval.py. The
symbol embed text is qualified_name + signature + doc + roux's generated NL
description — the richest text available, to give the dense arm its best shot.

Requires fastembed in a throwaway venv (measurement only, not a project dep):
    python3 -m venv /tmp/roux-dense-venv
    /tmp/roux-dense-venv/bin/pip install fastembed numpy
    /tmp/roux-dense-venv/bin/python bench/vocab_gap_dense.py [source ...]

Reports S Hit@1/5/10 + MRR per source and combined, next to the lexical
baseline (heldout_eval.py) and the reformulation arm (vocab_gap_reform.py).
"""
from __future__ import annotations

import glob
import json
import sqlite3
import sys
from collections import defaultdict
from pathlib import Path

import numpy as np
from fastembed import TextEmbedding

BENCH = Path(__file__).resolve().parent
sys.path.insert(0, str(BENCH))
import bench_match  # noqa: E402  # strict_match, mirrors heldout_eval

SNAPSHOTS = BENCH / "snapshots"
MODEL = "BAAI/bge-small-en-v1.5"
TOPK = 10


def load_s_queries(only: set[str] | None) -> dict[str, list[dict]]:
    by_src: dict[str, list[dict]] = defaultdict(list)
    for fp in sorted(glob.glob(str(BENCH / "gold_*.frozen.json"))):
        d = json.load(open(fp))
        src = d["source_name"]
        if only and src not in only:
            continue
        for q in d.get("queries", []):
            if q.get("bucket") == "S":
                by_src[src].append({"query": q["query"], "gold": q["gold"]})
    return by_src


def symbol_texts(snap: Path) -> list[tuple[str, str]]:
    """(qualified_name, embed_text) per code symbol. Excludes file/doc_section
    structural nodes (never gold) — a best-case pool for the dense arm."""
    con = sqlite3.connect(snap)
    rows = con.execute(
        "SELECT qualified_name, name, signature, doc, description FROM nodes "
        "WHERE kind NOT IN ('file', 'doc_section')"
    ).fetchall()
    con.close()
    out = []
    for qn, name, sig, doc, desc in rows:
        label = qn or name or ""
        text = " ".join(p for p in (label, sig, doc, desc) if p)
        out.append((label, text or label))
    return out


def normd(m: np.ndarray) -> np.ndarray:
    n = np.linalg.norm(m, axis=1, keepdims=True)
    n[n == 0] = 1.0
    return m / n


def main() -> None:
    only = set(sys.argv[1:]) or None
    by_src = load_s_queries(only)
    embedder = TextEmbedding(model_name=MODEL)

    tot = {"n": 0, "h1": 0, "h5": 0, "h10": 0, "mrr": 0.0}
    print(f"=== OFFLINE DENSE ARM ({MODEL}) — S bucket, CEILING ONLY ===")
    per_query = []
    for src in sorted(by_src):
        snap = SNAPSHOTS / f"{src}.sqlite"
        if not snap.exists():
            print(f"  {src}: snapshot missing, skip")
            continue
        labels_texts = symbol_texts(snap)
        labels = [lt[0] for lt in labels_texts]
        doc_emb = normd(np.array(list(embedder.embed([lt[1] for lt in labels_texts]))))
        qs = by_src[src]
        q_emb = normd(np.array(list(embedder.query_embed([q["query"] for q in qs]))))
        sims = q_emb @ doc_emb.T  # (nq, ndoc)
        d = {"n": 0, "h1": 0, "h5": 0, "h10": 0, "mrr": 0.0}
        for i, q in enumerate(qs):
            topk = np.argsort(-sims[i])[:TOPK]
            names = [labels[j] for j in topk]
            rank = next((r + 1 for r, nm in enumerate(names)
                         if any(bench_match.strict_match(nm, g) for g in q["gold"])), None)
            d["n"] += 1
            if rank:
                d["h1"] += rank <= 1
                d["h5"] += rank <= 5
                d["h10"] += rank <= 10
                d["mrr"] += 1.0 / rank
            per_query.append({"source": src, "query": q["query"], "gold": q["gold"], "rank": rank})
        for k in tot:
            tot[k] += d[k]
        print(f"  {src:8} n={d['n']:2}  Hit@1 {d['h1']:2}/{d['n']:<2} "
              f"Hit@5 {d['h5']:2}/{d['n']:<2} Hit@10 {d['h10']:2}/{d['n']:<2} MRR {d['mrr']/d['n']:.3f}")
    if tot["n"]:
        print(f"  {'COMBINED':8} n={tot['n']:2}  Hit@1 {tot['h1']:2}/{tot['n']:<2} "
              f"Hit@5 {tot['h5']:2}/{tot['n']:<2} Hit@10 {tot['h10']:2}/{tot['n']:<2} "
              f"MRR {tot['mrr']/tot['n']:.3f}")
    out = BENCH / "results" / "vocab_gap_dense.json"
    out.parent.mkdir(exist_ok=True)
    combined = {**tot, "mrr": tot["mrr"] / tot["n"] if tot["n"] else 0.0}
    json.dump({"model": MODEL, "topk": TOPK, "combined": combined, "per_query": per_query},
              open(out, "w"), indent=2)
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
