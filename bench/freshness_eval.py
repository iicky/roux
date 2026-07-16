#!/usr/bin/env python3
"""Freshness bench — the honest gate for the incremental-freshness epic.

The persona Hit@10 bench scores a STATIC snapshot and structurally cannot see this
epic's vision: after you edit code, does roux reflect the change, cheaply, without
returning ghosts? A real ranking fix once moved Hit@10 by 0.000. This
arm measures the two things that matter for "roux keeps agents on UP-TO-DATE code":

  1. FRESHNESS LATENCY — wall-clock to make an edit queryable, as a fraction of a
     full re-index. Today the only refresh path is a full `roux add`, so the ratio
     is ~1.0; when `roux update` lands this harness auto-uses it and the
     ratio should collapse. That ratio is the epic's headline number.
  2. POST-EDIT CORRECTNESS (pass/fail) — after add/rename/move/delete/body-shift, a
     query must return the CURRENT reality: the new symbol is findable, renamed and
     deleted symbols leave NO ghost, moved symbols report the new file, and a body
     edit that shifts lines yields the CURRENT line number, not a stale one. Stale
     locations are worse than grep (confidently-wrong = agent trust destroyed), so
     these are correctness gates, not perf nits.

Black-box: everything goes through the real `roux` binary (add / query --format json).
A full re-index is correct by construction, so this is GREEN today and becomes the
regression gate that a future incremental `roux update` must keep green.

Usage:
  python3 bench/freshness_eval.py            # run all scenarios, table + pass/fail
  python3 bench/freshness_eval.py --json     # also emit machine-readable results
Exit code is non-zero if any correctness check fails.
"""
from __future__ import annotations

import json
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ROUX = ROOT / "target" / "release" / "roux"

# ── fixture: a small multi-file Python package with distinctively-named symbols ──
FIXTURE: dict[str, str] = {
    "pkg/orders.py": (
        "def compute_order_total(items):\n"
        "    return sum(i.price for i in items)\n\n"
        "def apply_discount(total, pct):\n"
        "    return total * (1 - pct / 100)\n"
    ),
    "pkg/users.py": (
        "def find_user_by_email(email):\n"
        "    return _lookup(email)\n\n"
        "def _lookup(key):\n"
        "    return registry.get(key)\n"
    ),
    "pkg/textutil.py": (
        "def normalize_whitespace(s):\n"
        "    return ' '.join(s.split())\n"
    ),
}


def gen_filler(dst: Path, n_files: int) -> None:
    """Write n_files of unique-named functions so full-index time is stable and a
    whole-tree re-index is genuinely costly — the baseline a real incremental
    `roux update` must beat. Each function calls the next to exercise cross-file
    edges (resolve_references cost, which the update path must also pay)."""
    for i in range(n_files):
        lines = [f"# generated filler module {i}"]
        for j in range(6):
            lines += [f"def gen_{i}_fn_{j}(x):",
                      f"    return gen_{i}_fn_{(j + 1) % 6}(x) if x else {i * 6 + j}", ""]
        (dst / "pkg" / "gen" / f"m{i}.py").write_text("\n".join(lines) + "\n")


def write_fixture(dst: Path, n_filler: int = 0) -> None:
    if n_filler:
        (dst / "pkg" / "gen").mkdir(parents=True, exist_ok=True)
        gen_filler(dst, n_filler)
    for rel, body in FIXTURE.items():
        p = dst / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(body)


class HarnessError(RuntimeError):
    """A roux invocation failed — a setup/tooling problem, NOT a freshness result."""


def roux(*args: str, cwd: Path, check: bool = True) -> subprocess.CompletedProcess:
    proc = subprocess.run([str(ROUX), *args], cwd=cwd, capture_output=True, text=True, timeout=180)
    if check and proc.returncode != 0:
        raise HarnessError(f"roux {' '.join(args)} exited {proc.returncode}: {proc.stderr.strip()[:300]}")
    return proc


def index_full(cwd: Path) -> float:
    """Full (re)index of the working tree into .roux/db.sqlite; returns seconds."""
    t = time.perf_counter()
    roux("add", ".", "--local", "--lang", "python", "--name", "fixture", cwd=cwd)
    return time.perf_counter() - t


def update_available() -> bool:
    return subprocess.run([str(ROUX), "update", "--help"], capture_output=True, text=True).returncode == 0


def refresh(cwd: Path) -> tuple[float, str]:
    """Make edits queryable via the cheapest available path; returns (seconds, path)."""
    if update_available():  # when it lands
        t = time.perf_counter()
        roux("update", "--local", cwd=cwd)
        return time.perf_counter() - t, "update"
    return index_full(cwd), "full-reindex"


def query(cwd: Path, q: str, top: int = 20) -> list[dict]:
    proc = roux("query", q, "--local", "--format", "json", "--top", str(top), cwd=cwd)
    out = proc.stdout.strip()
    if not out:
        return []  # roux emits empty stdout + "No results found." on stderr for zero matches
    try:
        return json.loads(out).get("symbols", [])
    except json.JSONDecodeError as e:  # non-empty but malformed = genuine tooling failure
        raise HarnessError(f"roux query {q!r} returned non-JSON: {out[:200]}") from e


def stale_block(cwd: Path, q: str = "compute order total") -> dict:
    proc = roux("query", q, "--local", "--format", "json", "--top", "3", cwd=cwd)
    out = proc.stdout.strip()
    if not out:
        return {}
    try:
        return json.loads(out).get("stale") or {}
    except json.JSONDecodeError as e:
        raise HarnessError(f"roux query {q!r} (stale probe) returned non-JSON: {out[:200]}") from e


def find(symbols: list[dict], name: str) -> dict | None:
    for s in symbols:
        qn = s.get("qualified_name") or ""
        if s.get("name") == name or qn.split("::")[-1] == name:
            return s
    return None


def present(cwd: Path, q: str, name: str) -> dict | None:
    return find(query(cwd, q), name)


def line_of(path: Path, needle: str) -> int | None:
    for i, ln in enumerate(path.read_text().splitlines(), 1):
        if needle in ln:
            return i
    return None


# ── scenarios: (name, edit(dir)->None, check(dir)->list[(label, ok)]) ───────────
def s_add(d: Path):
    (d / "pkg/orders.py").write_text(
        (d / "pkg/orders.py").read_text() + "\ndef cancel_order(order_id):\n    return _void(order_id)\n"
    )

def c_add(d: Path):
    s = present(d, "cancel order", "cancel_order")
    return [("new symbol findable", s is not None),
            ("at correct file", bool(s) and s.get("file", "").endswith("orders.py"))]


def s_rename(d: Path):
    p = d / "pkg/orders.py"
    p.write_text(p.read_text().replace("compute_order_total", "calculate_invoice_total"))

def c_rename(d: Path):
    new = present(d, "calculate invoice total", "calculate_invoice_total")
    # ghost check: the old name must be gone from the index entirely
    ghost = find(query(d, "compute order total"), "compute_order_total") \
        or find(query(d, "compute_order_total"), "compute_order_total")
    return [("renamed symbol findable", new is not None),
            ("no ghost of old name", ghost is None)]


def s_move(d: Path):
    src = d / "pkg/textutil.py"
    body = src.read_text()
    src.write_text("def _stub():\n    return None\n")            # leave file, drop the symbol
    (d / "pkg/strings.py").write_text(body)                       # symbol reappears elsewhere

def c_move(d: Path):
    s = present(d, "normalize whitespace", "normalize_whitespace")
    return [("moved symbol findable", s is not None),
            ("reports NEW file (no stale path)", bool(s) and s.get("file", "").endswith("strings.py"))]


def s_delete(d: Path):
    p = d / "pkg/orders.py"
    p.write_text(re.sub(r"\ndef apply_discount.*?return total \* \(1 - pct / 100\)\n", "\n", p.read_text(), flags=re.S))

def c_delete(d: Path):
    ghost = find(query(d, "apply discount"), "apply_discount") \
        or find(query(d, "apply_discount"), "apply_discount")
    return [("deleted symbol leaves no ghost", ghost is None)]


def s_bodyshift(d: Path):
    p = d / "pkg/users.py"
    p.write_text("# padding\n" * 12 + p.read_text())              # push find_user_by_email down 12 lines

def c_bodyshift(d: Path):
    p = d / "pkg/users.py"
    want = line_of(p, "def find_user_by_email")
    s = present(d, "find user by email", "find_user_by_email")
    got = s.get("line") if s else None
    return [("symbol still findable", s is not None),
            (f"line current (want {want}, got {got})", got == want)]


SCENARIOS = [
    ("add", s_add, c_add),
    ("rename", s_rename, c_rename),
    ("move", s_move, c_move),
    ("delete", s_delete, c_delete),
    ("body-shift", s_bodyshift, c_bodyshift),
]


def run_scenario(name, edit, check, n_filler: int) -> dict:
    try:
        with tempfile.TemporaryDirectory(prefix=f"roux-fresh-{name}-") as td:
            d = Path(td)
            write_fixture(d, n_filler)
            full0 = index_full(d)                 # initial full index (baseline denominator)
            edit(d)
            staleness_seen = bool(stale_block(d))  # shipped guard should notice pre-refresh
            secs, path = refresh(d)
            checks = check(d)
    except HarnessError as e:
        return {"scenario": name, "error": str(e), "correct": False, "checks": []}
    ratio = secs / full0 if full0 else float("nan")
    ok = all(v for _, v in checks)
    return {"scenario": name, "refresh_path": path, "full_index_s": round(full0, 3),
            "refresh_s": round(secs, 3), "latency_ratio": round(ratio, 3),
            "staleness_flagged": staleness_seen, "correct": ok,
            "checks": [{"label": l, "ok": v} for l, v in checks]}


def main() -> None:
    if not ROUX.exists():
        sys.exit("roux release binary missing (cargo build --release)")
    emit_json = "--json" in sys.argv
    n_filler = 120
    for a in sys.argv[1:]:
        if a.startswith("--files="):
            n_filler = int(a.split("=", 1)[1])

    print("=" * 78)
    print("FRESHNESS EVAL — post-edit correctness + refresh latency")
    print(f"refresh path: {'roux update' if update_available() else 'full re-index (roux update not built yet)'}")
    print(f"fixture: {n_filler + len(FIXTURE)} files (+{n_filler} filler for a stable baseline)")
    print("=" * 78)

    results = [run_scenario(*s, n_filler) for s in SCENARIOS]
    all_ok = True
    print(f"\n  {'scenario':11} {'refresh':13} {'full_s':>7} {'refr_s':>7} {'ratio':>6} {'stale?':>6}  result")
    for r in results:
        all_ok &= r["correct"]
        if "error" in r:
            print(f"  {r['scenario']:11} {'ERROR':13} {'—':>7} {'—':>7} {'—':>6} {'—':>6}  ERROR")
            print(f"      ! {r['error']}")
            continue
        tag = "PASS" if r["correct"] else "FAIL"
        print(f"  {r['scenario']:11} {r['refresh_path']:13} {r['full_index_s']:7.3f} "
              f"{r['refresh_s']:7.3f} {r['latency_ratio']:6.2f} {str(r['staleness_flagged']):>6}  {tag}")
        for c in r["checks"]:
            if not c["ok"]:
                print(f"      ✗ {c['label']}")

    n_full = sum(1 for r in results if r.get("refresh_path") == "full-reindex")
    n_err = sum(1 for r in results if "error" in r)
    print(f"\n  latency ratio ~1.0 expected while refresh=full-reindex ({n_full}/{len(results)}); "
          "an incremental refresh path must collapse it.")
    print(f"  correctness: {'ALL PASS' if all_ok else 'FAILURES ABOVE'} "
          f"({sum(r['correct'] for r in results)}/{len(results)} scenarios"
          f"{f', {n_err} HARNESS ERROR' if n_err else ''})")

    if emit_json:
        out = ROOT / "bench" / "results"
        out.mkdir(parents=True, exist_ok=True)
        stamp = time.strftime("%Y%m%dT%H%M%S")
        p = out / f"freshness_{stamp}.json"
        p.write_text(json.dumps({"generated": stamp, "results": results}, indent=2) + "\n")
        print(f"\n  wrote {p}")

    sys.exit(0 if all_ok else 1)


if __name__ == "__main__":
    main()
