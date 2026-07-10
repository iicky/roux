#!/usr/bin/env python3
"""Shared persona-query parser for the bench harnesses.

Extracts the canonical persona query set straight out of tests/bench_personas.rs
(name/path/language + each query's expected symbols and Agent/Developer mode) so
consumers never drift from the Rust source of truth. Imported by
bench/token_economics.py.

This module used to also host a grep-vs-roux comparison; that has been retired.
Its whole-query grep arm was a strawman (`rg` on a full natural-language question
matches almost nothing), and its best-token arm graded grep on line CONTENT while
roux was graded on SYMBOL names — not comparable. The two honest comparisons live
elsewhere:
  - retrieval floor : bench/grep_floor_eval.py — a grep-savvy arm (distinctive
    token -> enclosing symbol) graded on symbols exactly like roux.
  - agent economics : bench/token_economics.py — a baseline agent (its normal
    tools, no roux) vs the same agent with roux, the comparison that matters for
    the real audience.
"""
from __future__ import annotations

import re
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
