#!/usr/bin/env python3
"""Regression tests for the token-economics success grader.

Guards two properties the grader must hold to produce publicly citable numbers:
it grades the FULL agent answer (not a 240-char preview), and it requires a
concrete file:line citation — so a disclaimer that merely echoes the gold symbol
name no longer scores as SUCCESS.

Run: python3 bench/test_token_economics_grader.py
"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from token_economics import _success  # noqa: E402

GOLD = ["SearcherBuilder"]

CASES = [
    # (description, answer, expected_success)
    (
        "full text past the old 240-char preview boundary still grades",
        "x" * 300 + " It is defined in crates/searcher/src/lib.rs:88 as SearcherBuilder.",
        True,
    ),
    (
        "refusal echoing the symbol name -> failure (no citation)",
        "I could not find SearcherBuilder anywhere in this codebase.",
        False,
    ),
    (
        "symbol named with a file:line citation -> success",
        "SearcherBuilder lives in src/searcher.rs:42.",
        True,
    ),
    (
        "symbol named but no location cited -> failure (citable-metric strictness)",
        "The SearcherBuilder type is what handles this.",
        False,
    ),
    (
        "citation present but gold symbol absent -> failure",
        "The relevant logic is in src/matcher.rs:10.",
        False,
    ),
    ("empty answer -> failure", "", False),
]


def main() -> None:
    ok = True
    for desc, answer, want in CASES:
        got = _success(answer, GOLD)
        status = "ok  " if got == want else "FAIL"
        ok &= got == want
        print(f"  [{status}] {desc}")
    print("\nPASS" if ok else "\nSOME TESTS FAILED")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
