"""Shared strict match predicate for the Python bench harnesses.

Word-boundary (token) matching, not any-substring: a gold hits a ranked symbol
when the gold's identifier tokens appear as a contiguous run inside the symbol
name's tokens. Tokenizing splits snake_case, kebab/dot separators, and camelCase
(including acronym boundaries like ``JSONParser`` -> [json, parser]). So
``search`` no longer matches ``research`` but ``JSON`` still matches ``BindJSON``.

Mirrors tests/common/mod.rs; keep the two in sync (unification tracked in
roux-mj3k).
"""

from __future__ import annotations


def tokens(s: str) -> list[str]:
    """Split an identifier into lowercased word tokens."""
    out: list[str] = []
    cur: list[str] = []
    for i, c in enumerate(s):
        if not c.isalnum():
            if cur:
                out.append("".join(cur))
                cur = []
            continue
        if cur:
            prev = s[i - 1]
            camel = c.isupper() and (prev.islower() or prev.isdigit())
            acronym = (
                c.isupper()
                and prev.isupper()
                and i + 1 < len(s)
                and s[i + 1].islower()
            )
            if camel or acronym:
                out.append("".join(cur))
                cur = []
        cur.append(c.lower())
    if cur:
        out.append("".join(cur))
    return out


def strict_match(name: str, gold: str) -> bool:
    """True when gold's token sequence appears contiguously within name's tokens."""
    g = tokens(gold)
    if not g:
        return False
    n = tokens(name)
    if len(g) > len(n):
        return False
    return any(n[i:i + len(g)] == g for i in range(len(n) - len(g) + 1))


def any_match(name: str, expected: list[str]) -> bool:
    return any(strict_match(name, e) for e in expected)
