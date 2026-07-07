#!/usr/bin/env python3
"""Harvest a blind, in-index gold query set from a persona repo's own issue history.

Motivation (roux-x6fs): the CI-gating query set is lexically self-fulfilling — the
target's name words are in the query — so it is Goodharted and cannot detect ranking
improvements. This tool builds a *held-out* set from signal the maintainer never wrote
to flatter roux: real user-reported issues, and the symbols the fix actually touched.

Pipeline (commit-first, robust to line drift):
  1. Load the snapshot symbol table (ground truth for what roux can retrieve).
  2. Walk commits up to the pinned ref; keep those that close an issue
     ("Fixes/Closes/Resolves #N" on a single line).
  3. From each fix commit's diff, extract touched symbol NAMES via git's builtin
     hunk-header driver + keyword-anchored def lines (per language), then pin them
     to snapshot symbols by (name, file_path). Name-based pinning is drift-robust:
     a symbol renamed/moved after the fix simply fails to pin (roux can't retrieve
     it either), so stale gold never leaks in.
  4. Fetch the referenced issue via `gh`; skip PRs (we want user NL, not maintainer NL).
  5. Query = issue TITLE only (bodies leak identifiers/paths/flags — advisory).
  6. Pre-score with the real roux binary against the snapshot to bucket hardness.
  7. Emit a DRAFT json (field shape mirrors bench/hard_queries_*.json; pairs with the
     x6fs eval harness bench/heldout_eval.py that reads gold via `roux query --db`) for review.

This is a *draft generator*, not an oracle. Gold is candidate-grade; a human freezes it
(roux-x6fs.3). Multi-language via the LANGS registry — Rust/Python/Go/TypeScript/C++.

Usage:
  python3 bench/harvest_persona_queries.py <persona> [--max-commits N] [--limit N]
  personas: ripgrep pandas gin remix Marlin
"""
from __future__ import annotations

import argparse
import json
import re
import sqlite3
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUX = ROOT / "target" / "release" / "roux"
SNAP = ROOT / "bench" / "snapshots"

# ── Per-language extraction config ──────────────────────────────────────────
# defs: keyword-anchored regexes. Safe to run on both the git hunk-header context
# (the enclosing definition, when a builtin diff driver exists) AND on changed
# +/- lines, because requiring a def keyword (fn/def/class/func/type/...) means
# they fire on DEFINITIONS, not call sites. Multi-alternative regexes return the
# first non-empty group. pin_gold's (name, file_path) match is the precision floor:
# a candidate only becomes gold if a symbol of that name is defined in that file.
RUST_DEF = re.compile(r"\b(?:fn|struct|enum|trait|type|macro_rules!)\s+([A-Za-z_][A-Za-z0-9_]*)")
RUST_IMPL = re.compile(r"\bimpl\b[^{]*?\bfor\b\s+([A-Za-z_]\w*)|\bimpl\s+([A-Za-z_]\w*)")
PY_DEF = re.compile(r"\b(?:def|class)\s+([A-Za-z_]\w*)")
GO_FUNC = re.compile(r"\bfunc\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)")
GO_TYPE = re.compile(r"\btype\s+([A-Za-z_]\w*)")
TS_FUNC = re.compile(r"\b(?:export\s+)?(?:default\s+)?(?:async\s+)?function\*?\s+([A-Za-z_$][\w$]*)")
TS_CLASS = re.compile(r"\b(?:export\s+)?(?:abstract\s+)?(?:class|interface|enum|namespace|type)\s+([A-Za-z_$][\w$]*)")
TS_ARROW = re.compile(
    r"\b(?:export\s+)?(?:default\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*"
    r"(?::[^=]+)?=\s*(?:async\s*)?(?:function\b|\([^)]*\)\s*(?::[^=]+)?=>|[A-Za-z_$][\w$]*\s*=>)"
)
# method/property inside a class body — extracted from the hunk CONTEXT only
# (a git-picked enclosing def line), never changed lines, so call sites don't leak.
TS_METHOD_CTX = re.compile(
    r"^\s*(?:public\s+|private\s+|protected\s+|static\s+|readonly\s+|async\s+|\*\s*|get\s+|set\s+)*"
    r"([A-Za-z_$][\w$]*)\s*[(<]"
)
# funcname pattern for a custom git driver (TS/JS have no builtin one): the enclosing
# line git shows in @@ hunk headers. Broad on purpose — the Python-side extractors +
# STOP_NAMES + pin_gold's file match reject the noise (e.g. `if (`, `for (`).
TS_XFUNCNAME = (
    r"^[\t ]*((export[\t ]+)?(default[\t ]+)?(abstract[\t ]+)?(async[\t ]+)?"
    r"(function[\t *]|class[\t ]|interface[\t ]|enum[\t ]|namespace[\t ]|type[\t ]|"
    r"(const|let|var)[\t ])|((public|private|protected|static|readonly|async|get|set)[\t ]+)*"
    r"[A-Za-z_$][A-Za-z0-9_$]*[\t ]*[(<]).*$"
)
# never a real symbol name; drops control-flow contexts that look like `name(`
STOP_NAMES = frozenset({
    "if", "for", "while", "switch", "catch", "return", "function", "await", "typeof",
    "new", "else", "do", "try", "in", "of", "this", "super", "yield", "throw", "case",
    "with", "delete", "void", "constructor",
})
# generic/god symbols make useless gold: every class has __init__, mega-constructors
# (from_low_args) & dispatchers get swept into unrelated fixes. Dropped from gold at pin
# time (roux-x6fs.3); per-persona extras live in PERSONAS[...]["skip_syms"].
GENERIC_SYM = re.compile(r"^__\w+__$")   # dunders
MEGA_SYM = frozenset({"from_low_args"})  # known cross-cutting god-constructors
CPP_KW = re.compile(r"\b(?:class|struct|enum|namespace|union)\s+([A-Za-z_]\w*)")
CPP_DEFINE = re.compile(r"^\s*#\s*define\s+([A-Za-z_]\w*)")
# enclosing function signature from git's cpp hunk driver: `<ret> Class::method(args) {`
CPP_FUNC_CTX = re.compile(r"\b([A-Za-z_]\w*)\s*\([^;{]*\)\s*(?:const)?\s*(?:\{|$)")

LANGS: dict[str, dict] = {
    "rust": {
        "exts": (".rs",),
        "driver": "rust",
        "test_path": re.compile(r"(^|/)(tests?|benches|benchmarks)(/|$)|_test\.rs$|(^|/)tests\.rs$"),
        "context_defs": [RUST_DEF, RUST_IMPL],
        "line_defs": [RUST_DEF],
        "is_test_sym": lambda qn, n: "::tests::" in qn or "::test::" in qn or n.startswith("test_"),
        # ripgrep generates flag doc-strings; edits there are prose, not a code answer.
        "doc_sym": re.compile(r"^(?:doc_(?:long|short|choices|category|name|variable)|completion_type)$"),
        "doc_file": re.compile(r"(^|/)flags/doc/"),
    },
    "python": {
        "exts": (".py",),
        "driver": "python",
        "test_path": re.compile(r"(^|/)tests?/|(^|/)test_[^/]*\.py$|_test\.py$|(^|/)conftest\.py$"),
        "context_defs": [PY_DEF],
        "line_defs": [PY_DEF],
        "is_test_sym": lambda qn, n: n.startswith("test_") or n.startswith("Test"),
        "doc_sym": None,
        "doc_file": None,
    },
    "go": {
        "exts": (".go",),
        "driver": "golang",
        "test_path": re.compile(r"_test\.go$|(^|/)testdata/"),
        "context_defs": [GO_FUNC, GO_TYPE],
        "line_defs": [GO_FUNC, GO_TYPE],
        "is_test_sym": lambda qn, n: n.startswith(("Test", "Benchmark", "Example", "Fuzz")),
        "doc_sym": None,
        "doc_file": None,
    },
    "typescript": {
        "exts": (".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"),
        "driver": "tsfunc",  # custom xfuncname (see enable_hunks) — TS/JS lack a builtin driver
        "xfuncname": TS_XFUNCNAME,
        "test_path": re.compile(
            r"(^|/)__tests__/|(^|/)integration/|\.test\.[cm]?[tj]sx?$|-test\.[cm]?[tj]sx?$|\.spec\.[cm]?[tj]sx?$"
        ),
        "context_defs": [TS_FUNC, TS_CLASS, TS_ARROW, TS_METHOD_CTX],
        "line_defs": [TS_FUNC, TS_CLASS, TS_ARROW],  # NOT TS_METHOD_CTX: bare name( matches calls
        "is_test_sym": lambda qn, n: False,
        "doc_sym": None,
        "doc_file": None,
    },
    "cpp": {
        "exts": (".cpp", ".cc", ".cxx", ".c", ".h", ".hpp", ".hh", ".hxx", ".ino"),
        "driver": "cpp",
        "test_path": re.compile(r"(^|/)tests?/"),
        "context_defs": [CPP_KW, CPP_DEFINE, CPP_FUNC_CTX],
        "line_defs": [CPP_KW, CPP_DEFINE],  # NOT CPP_FUNC_CTX: bare name( matches call sites
        "is_test_sym": lambda qn, n: False,
        "doc_sym": None,
        "doc_file": None,
    },
}

# Per-persona config. Refs mirror bench/build_persona_indexes.sh / persona-bench.yml.
# `clone` is a DEEP clone (full history) — the shallow index clones under
# /tmp/roux-sources cannot be walked. `src_prefixes` focuses the diff to real source
# (drops docs/benchmarks/ci); empty = accept any path with a source extension.
PERSONAS = {
    "ripgrep": {"repo": "BurntSushi/ripgrep", "ref": "14.1.1", "lang": "rust",
                "clone": "/tmp/rg-bench", "snapshot": SNAP / "ripgrep.sqlite",
                "src_prefixes": ("crates/",)},
    "pandas": {"repo": "pandas-dev/pandas", "ref": "v2.2.3", "lang": "python",
               "clone": "/tmp/pandas-bench", "snapshot": SNAP / "pandas.sqlite",
               "src_prefixes": ("pandas/",)},
    "gin": {"repo": "gin-gonic/gin", "ref": "v1.10.0", "lang": "go",
            "clone": "/tmp/gin-bench", "snapshot": SNAP / "gin.sqlite",
            "src_prefixes": (), "skip_syms": {"head"}},  # head: tiny comma-split helper, noise gold
    "remix": {"repo": "remix-run/remix", "ref": "remix@2.13.1", "lang": "typescript",
              "clone": "/tmp/remix-bench", "snapshot": SNAP / "remix.sqlite",
              "src_prefixes": ("packages/",)},
    "Marlin": {"repo": "MarlinFirmware/Marlin", "ref": "2.1.2.5", "lang": "cpp",
               "clone": "/tmp/marlin-bench", "snapshot": SNAP / "Marlin.sqlite",
               "src_prefixes": ("Marlin/",)},
}

CLOSER_RE = re.compile(r"(?im)^.*\b(?:fix(?:e[sd])?|clos(?:e[sd])?|resolv(?:e[sd])?)\b[^\n]*?#(\d+)")
HUNK_RE = re.compile(r"^@@ .*? @@ ?(.*)$")
# generic issue-title prefixes to strip so the query reads like a user question
GENERIC_PREFIX_RE = re.compile(
    r"^(?:bug|feature request|feature|feat|enhancement|question|proposal|docs?|perf|regression)\s*[:\-]\s*",
    re.I,
)


def git(clone: str, *args: str) -> str:
    # firmware/C++ sources carry non-UTF-8 bytes (e.g. 0xb0 for °); decode leniently
    return subprocess.run(
        ["git", "-C", clone, *args],
        capture_output=True, text=True, errors="replace",
    ).stdout


def enable_hunks(clone: str, cfg: dict) -> None:
    """Activate a hunk-header driver so @@ contexts name the enclosing definition.
    Builtin drivers (rust/python/golang/cpp) just need the gitattributes mapping;
    a custom driver (TS/JS) also needs its xfuncname registered in the clone."""
    lang = LANGS[cfg["lang"]]
    driver = lang["driver"]
    if not driver:
        return
    if lang.get("xfuncname"):
        git(clone, "config", f"diff.{driver}.xfuncname", lang["xfuncname"])
    attr = Path(clone) / ".git" / "info" / "attributes"
    attr.write_text("".join(f"*{ext} diff={driver}\n" for ext in lang["exts"]))


def load_symbols(snapshot: Path):
    """name -> list[(qualified_name, file_path, kind)]."""
    con = sqlite3.connect(f"file:{snapshot}?mode=ro", uri=True)
    rows = con.execute("SELECT name, qualified_name, file_path, kind FROM nodes").fetchall()
    con.close()
    by_name: dict[str, list[tuple[str, str, str]]] = {}
    for name, qn, fp, kind in rows:
        by_name.setdefault(name, []).append((qn, fp, kind))
    return by_name


def changed_source_files(clone: str, sha: str, cfg: dict) -> list[str]:
    lang = LANGS[cfg["lang"]]
    prefixes = tuple(cfg.get("src_prefixes") or ())
    files = git(clone, "show", "--name-only", "--pretty=format:", sha).split()
    out = []
    for f in files:
        if not f.endswith(lang["exts"]):
            continue
        if prefixes and not f.startswith(prefixes):
            continue
        if lang["test_path"].search(f):
            continue
        if lang["doc_file"] and lang["doc_file"].search(f):
            continue
        out.append(f)
    return out


def touched_symbols(clone: str, sha: str, keep_files: set[str], cfg: dict) -> dict[str, set[str]]:
    """Return {file_path: {candidate short symbol names}} from the diff."""
    lang = LANGS[cfg["lang"]]
    show = git(clone, "show", "--unified=0", sha)
    cur_file = None
    per_file: dict[str, set[str]] = {}

    def harvest(text: str, regexes: list[re.Pattern]) -> None:
        for rx in regexes:
            for m in rx.finditer(text):
                name = next((g for g in m.groups() if g), None)
                if name and name not in STOP_NAMES:
                    per_file[cur_file].add(name)

    for line in show.splitlines():
        if line.startswith("+++ b/"):
            f = line[6:]
            cur_file = f if f in keep_files else None
            if cur_file:
                per_file.setdefault(cur_file, set())
            continue
        if cur_file is None:
            continue
        m = HUNK_RE.match(line)
        if m:  # enclosing item from the language hunk driver
            harvest(m.group(1), lang["context_defs"])
            continue
        if line[:1] in "+-" and not line.startswith(("+++", "---")):  # def on changed line
            harvest(line[1:], lang["line_defs"])
    return per_file


def pin_gold(per_file: dict[str, set[str]], by_name, cfg: dict) -> tuple[list[str], list[str]]:
    """Pin candidate names to snapshot qualified_names by (name, file_path).
    Returns (gold_qualified_names, gold_short_names) excluding test/doc/generic symbols."""
    lang = LANGS[cfg["lang"]]
    doc_sym = lang["doc_sym"]
    is_test = lang["is_test_sym"]
    skip = MEGA_SYM | set(cfg.get("skip_syms") or ())
    gold_qn: list[str] = []
    gold_short: set[str] = set()
    for fp, names in per_file.items():
        for nm in names:
            if doc_sym and doc_sym.match(nm):
                continue
            if GENERIC_SYM.match(nm) or nm in skip:  # dunders / god-constructors: useless gold
                continue
            for qn, sfp, _kind in by_name.get(nm, []):
                if sfp == fp and not is_test(qn, nm):
                    gold_qn.append(qn)
                    gold_short.add(nm)
    return sorted(set(gold_qn)), sorted(gold_short)


def fetch_issue(repo: str, num: int) -> tuple[str, dict | None]:
    """Return (status, issue). status: ok | pr | notfound | err (gh/API failure)."""
    proc = subprocess.run(
        ["gh", "api", f"repos/{repo}/issues/{num}"],
        capture_output=True, text=True,
    )
    if proc.returncode != 0:
        # 404 => genuinely-missing issue; anything else (rate limit, network) is an error
        return ("notfound" if "404" in proc.stderr else "err", None)
    try:
        obj = json.loads(proc.stdout)
    except json.JSONDecodeError:
        return ("err", None)
    if "pull_request" in obj:  # it's a PR, not a user issue
        return ("pr", None)
    return ("ok", obj)


def clean_title(title: str, persona: str) -> str:
    t = title.strip().strip("`")
    aliases = {"ripgrep": "rg|ripgrep|regex", "Marlin": "marlin"}.get(persona, re.escape(persona))
    t = re.sub(rf"^(?:{aliases})\s*[:\-]\s*", "", t, flags=re.I)
    t = GENERIC_PREFIX_RE.sub("", t)
    return t.rstrip(".").strip()


def roux_rank(query: str, snapshot: Path, gold_short: list[str], top: int = 10):
    proc = subprocess.run(
        [str(ROUX), "query", query, "--db", str(snapshot), "--format", "json", "--top", str(top)],
        capture_output=True, text=True, timeout=120,
    )
    try:
        # roux appends graph neighbors past --top, so slice to top-K: a hit beyond K
        # is a miss@K (bucket S), consistent with bench/heldout_eval.py.
        names = [
            s.get("qualified_name") or s.get("name", "")
            for s in json.loads(proc.stdout).get("symbols", [])
        ][:top]
    except json.JSONDecodeError:
        return None
    for i, n in enumerate(names):
        if any(g.lower() in n.lower() for g in gold_short):
            return i + 1
    return None


def tokset(s: str) -> set[str]:
    parts = re.split(r"[^A-Za-z0-9]+", s)
    out = set()
    for p in parts:
        for w in re.findall(r"[A-Z]?[a-z0-9]+|[A-Z]+(?![a-z])", p):
            if len(w) >= 3:
                out.add(w.lower())
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("persona", choices=sorted(PERSONAS))
    ap.add_argument("--max-commits", type=int, default=150,
                    help="scan this many most-recent issue-closing commits")
    ap.add_argument("--limit", type=int, default=0, help="cap emitted queries (0=all)")
    ap.add_argument("--max-files", type=int, default=4,
                    help="skip fixes touching more source files than this (too broad to map title->symbol)")
    ap.add_argument("--out", default="", help="output path (default bench/gold_<persona>.draft.json)")
    args = ap.parse_args()

    cfg = PERSONAS[args.persona]
    clone, ref, snapshot, repo = cfg["clone"], cfg["ref"], cfg["snapshot"], cfg["repo"]
    if not Path(clone, ".git").exists():
        sys.exit(f"clone missing: {clone} "
                 f"(git clone --filter=blob:none --branch {ref} https://github.com/{repo} {clone})")
    if git(clone, "rev-list", "--count", ref).strip() in ("", "1"):
        sys.exit(f"clone at {clone} is shallow/refless — need full history for `git log {ref}`")
    if not snapshot.exists():
        sys.exit(f"snapshot missing: {snapshot}")
    if not ROUX.exists():
        sys.exit("roux release binary missing (cargo build --release)")

    enable_hunks(clone, cfg)
    by_name = load_symbols(snapshot)
    print(f"[{args.persona}] snapshot: {sum(len(v) for v in by_name.values())} symbols, "
          f"{len(by_name)} distinct names")

    log = git(clone, "log", ref, "--pretty=%H%x1f%s%x1f%b%x1e")
    commits = [c for c in log.split("\x1e") if c.strip()]

    seen_issue: set[int] = set()
    out_queries: list[dict] = []
    stats = {"pr": 0, "notfound": 0, "err": 0}
    scanned = 0
    for c in commits:
        parts = c.strip().split("\x1f")
        if len(parts) < 3:
            continue
        h, s, b = parts[0].strip(), parts[1], "\x1f".join(parts[2:])
        text = s + "\n" + b
        nums = [int(n) for n in CLOSER_RE.findall(text)]
        if not nums:
            continue
        scanned += 1
        if scanned > args.max_commits:
            break
        src = changed_source_files(clone, h, cfg)
        if not src or len(src) > args.max_files:
            continue
        per_file = touched_symbols(clone, h, set(src), cfg)
        gold_qn, gold_short = pin_gold(per_file, by_name, cfg)
        if not gold_qn:
            continue
        for num in nums:
            if num in seen_issue:
                continue
            status, issue = fetch_issue(repo, num)
            if status != "ok":
                stats[status] = stats.get(status, 0) + 1
                if status != "err":  # don't burn the issue number on a transient API failure
                    seen_issue.add(num)
                continue
            seen_issue.add(num)
            query = clean_title(issue["title"], args.persona)
            rank = roux_rank(query, snapshot, gold_short)
            leaks = bool(tokset(query) & {w for g in gold_short for w in tokset(g)})
            if rank == 1:
                bucket = "L"      # lexical/easy: BM25 top-1
            elif rank:
                bucket = "G"      # reachable but re-ranked (graph/tail) — the interesting middle
            else:
                bucket = "S"      # miss @10 — semantic/vocab gap (the anti-Goodhart core)
            out_queries.append({
                "id": f"{args.persona}-{num}",
                "bucket": bucket,
                "query": query,
                "gold": gold_qn[:5],
                "roux_rank": rank,
                "query_leaks_gold": leaks,
                "provenance": {
                    "issue": num,
                    "issue_url": issue["html_url"],
                    "labels": [l["name"] for l in issue.get("labels", [])],
                    "fix_commit": h[:12],
                    "changed_files": src,
                    "gold_candidates": gold_short,
                },
                "context": (issue.get("body") or "")[:280].replace("\r", " ").replace("\n", " "),
            })
            if args.limit and len(out_queries) >= args.limit:
                break
        if args.limit and len(out_queries) >= args.limit:
            break

    out_queries.sort(key=lambda q: q["provenance"]["issue"])
    dist = {k: sum(1 for q in out_queries if q["bucket"] == k) for k in ("L", "G", "S")}
    doc = {
        "_comment": (
            f"DRAFT harvested query set for roux-x6fs ({args.persona}) — NOT frozen, needs human "
            "review. Queries are real user issue TITLES from the persona repo; gold = qualified_name "
            "substrings pinned to the fix commit's touched symbols, validated against the snapshot "
            "index. bucket: L=roux top-1 (lexical), G=roux ranks 2-10 (graph/tail), S=roux misses@10 "
            "(vocab/semantic gap — the anti-Goodhart core). query_leaks_gold flags titles that already "
            "contain a gold identifier (should be pruned/relabeled). Review: drop off-topic/feature-"
            "request titles, tighten gold, then freeze (roux-x6fs.3)."
        ),
        "source_name": args.persona,
        "index_path": str(snapshot),
        "pinned_ref": ref,
        "distribution": dist,
        "queries": out_queries,
    }
    out = Path(args.out) if args.out else ROOT / "bench" / f"gold_{args.persona}.draft.json"
    out.write_text(json.dumps(doc, indent=2) + "\n")
    print(f"[{args.persona}] wrote {len(out_queries)} draft queries -> {out}")
    print(f"[{args.persona}] buckets: {dist}  "
          f"(leak-flagged: {sum(1 for q in out_queries if q['query_leaks_gold'])})")
    print(f"[{args.persona}] gh skips: {stats['pr']} PRs, {stats['notfound']} not-found, "
          f"{stats['err']} API errors")
    if stats["err"]:
        print(f"[{args.persona}] WARNING: {stats['err']} gh API errors (rate limit?) — "
              "yield may be undercounted; re-run to recover those issues.")


if __name__ == "__main__":
    main()
