#!/usr/bin/env python3
"""Blast-radius head-to-head on the frozen polars fix-PR set.

Question: given ONE prod file a fix touched (the seed), how well does each
arm predict the OTHER prod .rs files the same fix touched (the gold)?
Direction is strictly reverse: who references the seed's symbols — not what
the seed depends on.

Trials: rotation protocol. Every prod file of every multi-file PR serves as
seed once; gold = its siblings. PRs with >= WHALE_THRESHOLD prod files are
excluded from the primary metric and reported separately (mechanical
fan-out / multi-cause edits, not dependency radius).

Pre-registered caveat: gold is a noisy SUBSET of true blast radius — files
that could break but were not touched are absent, and some co-changes are
parallel edits rather than dependency-linked. Absolute recall is expected to
be modest for every arm; the between-arm comparison is the finding.

Arms (each emits results/blast_<arm>.jsonl: {pr, seed, ranked: [top 20]}):
  roux  — reverse edges from .roux/db.sqlite (inbound calls/imports/type_ref/
          references/implements to the seed file's nodes), files ranked by
          inbound-edge count.
          Depth conditions (PRE-REGISTERED): every graph arm is emitted at
          depth-1 (<arm>: direct reverse edges) AND depth-2 (<arm>2: one
          extra reverse hop, weighted DECAY=0.25), no post-hoc selection.
          Live arms (grep, lsp) are single-hop by construction — depth-2
          graphs vs single-hop live arms IS the multi-hop hypothesis.
  grep  — strong lexical floor (PRE-REGISTERED PRIMARY FLOOR): pub symbols
          regex-extracted from the seed, files ranked by IDF-weighted
          word-boundary hits over crates/*.rs.
  grepraw — same symbol set, raw occurrence counts (no IDF). Reported
          alongside grep; neither is selected post-hoc.
  lsp   — rust-analyzer textDocument/references over the same pub-symbol set
          (symbol parity with the grep arm), files ranked by reference count.
  codegraph / gitnexus — external drivers, same output contract.

Usage:
  python3 bench/blast_radius_eval.py gen
  python3 bench/blast_radius_eval.py roux|grep|lsp
  python3 bench/blast_radius_eval.py score
"""

import json
import os
import re
import sqlite3
import subprocess
import sys
import time
from collections import Counter, defaultdict
from pathlib import Path

BENCH = Path(__file__).resolve().parent
REPO = Path(os.environ.get("POLARS_REPO", "/tmp/polars-bench"))
CRATES = REPO / "crates"
FROZEN = BENCH / "external_polars.frozen.json"
TRIALS = BENCH / "blast_radius.trials.json"
RESULTS = BENCH / "results"
WHALE_THRESHOLD = 10
TOP_K = 20

# ---------------------------------------------------------------- trials

def is_test_path(p: str) -> bool:
    name = p.rsplit("/", 1)[-1]
    return (
        "/tests/" in p
        or p.startswith("tests/")
        or "/benches/" in p
        or name.startswith("test_")
        or name.endswith("_test.rs")
    )


def gen_trials():
    frozen = json.loads(FROZEN.read_text())
    trials, skipped_missing = [], 0
    for q in frozen["queries"]:
        prod = [f for f in q["changed_rs_crates"] if not is_test_path(f)]
        # Gold stays intact even when a sibling is not on disk at the pinned
        # ref (e.g. a file the fix itself added): graph/grep arms may still
        # surface it if it existed before. Only the SEED must exist on disk.
        if len(prod) < 2:
            continue
        whale = len(prod) >= WHALE_THRESHOLD
        for seed in prod:
            if not (CRATES / seed).exists():
                skipped_missing += 1
                continue
            trials.append({
                "pr": q["number"],
                "seed": seed,
                "gold": sorted(set(prod) - {seed}),
                "whale": whale,
            })
    out = {
        "_comment": (
            "Rotation trials for the blast-radius head-to-head. Seed = one "
            "prod .rs file changed by a polars fix PR (frozen set, pinned "
            f"ref {frozen['pinned_ref'][:12]}); gold = sibling prod files of "
            "the same PR. Test-path files dropped; PRs with >= "
            f"{WHALE_THRESHOLD} prod files flagged whale and excluded from "
            "the primary metric."
        ),
        "pinned_ref": frozen["pinned_ref"],
        "n_trials": len(trials),
        "n_prs": len({t["pr"] for t in trials}),
        "trials": trials,
    }
    TRIALS.write_text(json.dumps(out, indent=1))
    n_whale = sum(1 for t in trials if t["whale"])
    print(
        f"wrote {TRIALS.name}: {len(trials)} trials ({n_whale} whale) across "
        f"{out['n_prs']} PRs; {skipped_missing} seeds skipped (not on disk)"
    )


def load_trials():
    return json.loads(TRIALS.read_text())["trials"]


def emit(arm: str, rows: list):
    RESULTS.mkdir(exist_ok=True)
    path = RESULTS / f"blast_{arm}.jsonl"
    with path.open("w") as fh:
        for r in rows:
            fh.write(json.dumps(r) + "\n")
    print(f"wrote {path} ({len(rows)} rows)")


# ---------------------------------------------------------------- roux arm

ROUX_KINDS = ("calls", "imports", "type_ref", "references", "implements", "exports")
DECAY = 0.25  # frozen weight for the second reverse hop in <arm>2 conditions


def run_roux(depth: int):
    con = sqlite3.connect(REPO / ".roux" / "db.sqlite")
    kinds_ph = ",".join("?" * len(ROUX_KINDS))
    rows = []
    for t in load_trials():
        seed = t["seed"]
        seed_ids = [r[0] for r in con.execute(
            "SELECT id FROM nodes WHERE file_path = ?", (seed,))]
        scores = defaultdict(float)
        hop1_ids = set()
        if seed_ids:
            ph = ",".join("?" * len(seed_ids))
            q = (
                f"SELECT e.from_id, n.file_path FROM edges e "
                f"JOIN nodes n ON n.id = e.from_id "
                f"WHERE e.to_id IN ({ph}) AND e.kind IN ({kinds_ph}) "
                f"AND n.file_path != ?"
            )
            for fid, fp in con.execute(q, [*seed_ids, *ROUX_KINDS, seed]):
                hop1_ids.add(fid)
                if fp.endswith(".rs"):
                    scores[fp] += 1.0
        if depth == 2 and hop1_ids:
            ids = list(hop1_ids)
            for i in range(0, len(ids), 500):
                chunk = ids[i:i + 500]
                ph = ",".join("?" * len(chunk))
                q2 = (
                    f"SELECT n.file_path, COUNT(*) FROM edges e "
                    f"JOIN nodes n ON n.id = e.from_id "
                    f"WHERE e.to_id IN ({ph}) AND e.kind IN ({kinds_ph}) "
                    f"AND n.file_path != ? GROUP BY n.file_path"
                )
                for fp, c in con.execute(q2, [*chunk, *ROUX_KINDS, seed]):
                    if fp.endswith(".rs"):
                        scores[fp] += DECAY * c
        ranked = [f for f, _ in
                  sorted(scores.items(), key=lambda kv: -kv[1])[:TOP_K]]
        rows.append({"pr": t["pr"], "seed": seed, "ranked": ranked})
    emit("roux" if depth == 1 else "roux2", rows)


# ----------------------------------------------------------- codegraph arm

CG_DB = REPO / ".codegraph" / "codegraph.db"


def run_codegraph(depth: int):
    con = sqlite3.connect(f"file:{CG_DB}?mode=ro", uri=True)
    rows = []
    for t in load_trials():
        seed = f"crates/{t['seed']}"
        scores = defaultdict(float)
        hop1_ids = set()
        q = (
            "SELECT e.source, src.file_path FROM edges e "
            "JOIN nodes tgt ON tgt.id = e.target "
            "JOIN nodes src ON src.id = e.source "
            "WHERE tgt.file_path = ? AND e.kind != 'contains' "
            "AND src.file_path != ?"
        )
        for sid, fp in con.execute(q, (seed, seed)):
            hop1_ids.add(sid)
            if fp.endswith(".rs"):
                scores[fp] += 1.0
        if depth == 2 and hop1_ids:
            ids = list(hop1_ids)
            for i in range(0, len(ids), 500):
                chunk = ids[i:i + 500]
                ph = ",".join("?" * len(chunk))
                q2 = (
                    f"SELECT src.file_path, COUNT(*) FROM edges e "
                    f"JOIN nodes src ON src.id = e.source "
                    f"WHERE e.target IN ({ph}) AND e.kind != 'contains' "
                    f"AND src.file_path != ? GROUP BY src.file_path"
                )
                for fp, c in con.execute(q2, [*chunk, seed]):
                    if fp.endswith(".rs"):
                        scores[fp] += DECAY * c
        ranked = [f.removeprefix("crates/") for f, _ in
                  sorted(scores.items(), key=lambda kv: -kv[1])[:TOP_K]]
        rows.append({"pr": t["pr"], "seed": t["seed"], "ranked": ranked})
    emit("codegraph" if depth == 1 else "codegraph2", rows)


# ------------------------------------------------------------ gitnexus arm

def run_gitnexus():
    # `gitnexus impact` requires a symbol positional; for file-level reverse
    # deps the project's own recommended recipe is direct Cypher over the
    # LadybugDB graph. Reference set = ALL semantic relation types (more
    # generous than the impact tool's defaults, which exclude ACCESSES).
    rels = ("['CALLS','ACCESSES','IMPORTS','USES','METHOD_IMPLEMENTS',"
            "'IMPLEMENTS','METHOD_OVERRIDES','EXTENDS']")
    env = {**os.environ, "SCARF_ANALYTICS": "false"}

    def cypher(q: str) -> Counter:
        out = subprocess.run(
            ["gitnexus", "cypher", q, "--repo", "polars"],
            capture_output=True, text=True, timeout=300, env=env)
        scores = Counter()
        stdout = out.stdout.strip()
        if not stdout or stdout.startswith("["):
            return scores  # zero rows: CLI prints a bare JSON array
        data = json.loads(stdout[stdout.index("{"):])
        for line in data.get("markdown", "").splitlines()[2:]:
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if len(cells) != 2:
                continue
            fp = cells[0].removeprefix("crates/")
            if fp.endswith(".rs"):
                scores[fp] += int(cells[1])
        return scores

    trials = load_trials()
    seeds = sorted({t["seed"] for t in trials})
    cache = {}
    for i, seed in enumerate(seeds, 1):
        p = f"crates/{seed}"
        d1, d2 = Counter(), Counter()
        try:
            d1 = cypher(
                f"MATCH (caller)-[r:CodeRelation]->(def) "
                f"WHERE def.filePath = '{p}' AND caller.filePath <> '{p}' "
                f"AND r.type IN {rels} "
                f"RETURN caller.filePath AS fp, count(*) AS c")
            d2 = cypher(
                f"MATCH (a)-[r1:CodeRelation]->(b)-[r2:CodeRelation]->(def) "
                f"WHERE def.filePath = '{p}' AND b.filePath <> '{p}' "
                f"AND a.filePath <> '{p}' AND r1.type IN {rels} "
                f"AND r2.type IN {rels} "
                f"RETURN a.filePath AS fp, count(*) AS c")
        except Exception as e:
            print(f"  ! {seed}: {e}", file=sys.stderr)
        cache[seed] = (d1, d2)
        if i % 20 == 0:
            print(f"  {i}/{len(seeds)} seeds")
    rows1, rows2 = [], []
    for t in trials:
        d1, d2 = cache[t["seed"]]
        comb = defaultdict(float)
        for f, c in d1.items():
            comb[f] += c
        for f, c in d2.items():
            comb[f] += DECAY * c
        rows1.append({"pr": t["pr"], "seed": t["seed"],
                      "ranked": [f for f, _ in d1.most_common(TOP_K)]})
        rows2.append({"pr": t["pr"], "seed": t["seed"],
                      "ranked": [f for f, _ in sorted(
                          comb.items(), key=lambda kv: -kv[1])[:TOP_K]]})
    emit("gitnexus", rows1)
    emit("gitnexus2", rows2)


# ---------------------------------------------------------------- grep arm

PUB_RE = re.compile(
    r"^\s*pub(?:\([^)]*\))?\s+(?:unsafe\s+|async\s+|const\s+|extern\s+\"[^\"]*\"\s+)*"
    r"(?:fn|struct|enum|trait|type|const|static|mod|union)\s+([A-Za-z_]\w*)",
    re.M,
)
MACRO_RE = re.compile(r"^\s*macro_rules!\s+([A-Za-z_]\w*)", re.M)
WORD_RE = re.compile(r"[A-Za-z_]\w*")
MAX_SYMBOLS = 15


def seed_symbols(seed: str) -> list:
    text = (CRATES / seed).read_text(errors="replace")
    syms, seen = [], set()
    for m in list(PUB_RE.finditer(text)) + list(MACRO_RE.finditer(text)):
        s = m.group(1)
        if s not in seen:
            seen.add(s)
            syms.append(s)
    return syms[:MAX_SYMBOLS]


def build_token_index():
    """word -> Counter(file -> occurrences) over all crates/*.rs."""
    index = defaultdict(Counter)
    for path in CRATES.rglob("*.rs"):
        rel = str(path.relative_to(CRATES))
        for w in WORD_RE.findall(path.read_text(errors="replace")):
            index[w][rel] += 1
    return index


def run_grep(weighted: bool):
    t0 = time.time()
    index = build_token_index()
    print(f"token index over crates/*.rs in {time.time() - t0:.1f}s")
    rows = []
    for t in load_trials():
        seed = t["seed"]
        scores = defaultdict(float)
        for sym in seed_symbols(seed):
            hits = index.get(sym)
            if not hits:
                continue
            df = len(hits)  # files containing the symbol
            for f, c in hits.items():
                if f != seed:
                    scores[f] += c / df if weighted else c
        ranked = [f for f, _ in sorted(scores.items(), key=lambda kv: -kv[1])[:TOP_K]]
        rows.append({"pr": t["pr"], "seed": seed, "ranked": ranked})
    emit("grep" if weighted else "grepraw", rows)


# ---------------------------------------------------------------- lsp arm

class LspClient:
    def __init__(self, root: Path):
        self.proc = subprocess.Popen(
            ["rust-analyzer"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, cwd=root,
        )
        self.root = root
        self.next_id = 1
        self.active_progress = set()
        self.saw_progress = False

    def _send(self, msg: dict):
        data = json.dumps(msg).encode()
        self.proc.stdin.write(
            b"Content-Length: %d\r\n\r\n%s" % (len(data), data))
        self.proc.stdin.flush()

    def _recv(self):
        headers = {}
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise EOFError("rust-analyzer exited")
            line = line.strip()
            if not line:
                break
            k, _, v = line.partition(b":")
            headers[k.lower()] = v.strip()
        body = self.proc.stdout.read(int(headers[b"content-length"]))
        return json.loads(body)

    def _track(self, msg):
        if msg.get("method") == "window/workDoneProgress/create":
            self._send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
            return
        if msg.get("method") == "$/progress":
            tok = msg["params"]["token"]
            kind = msg["params"]["value"].get("kind")
            self.saw_progress = True
            if kind == "end":
                self.active_progress.discard(tok)
            else:
                self.active_progress.add(tok)
        if msg.get("method") in ("workspace/configuration",):
            self._send({"jsonrpc": "2.0", "id": msg["id"],
                        "result": [None] * len(msg["params"]["items"])})
        if msg.get("method") == "client/registerCapability":
            self._send({"jsonrpc": "2.0", "id": msg["id"], "result": None})

    def request(self, method: str, params: dict, timeout: float = 120):
        rid = self.next_id
        self.next_id += 1
        self._send({"jsonrpc": "2.0", "id": rid,
                    "method": method, "params": params})
        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = self._recv()
            if msg.get("id") == rid and ("result" in msg or "error" in msg):
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg["result"]
            self._track(msg)
        raise TimeoutError(method)

    def notify(self, method: str, params: dict):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def initialize(self):
        uri = self.root.as_uri()
        self.request("initialize", {
            "processId": os.getpid(),
            "rootUri": uri,
            "workspaceFolders": [{"uri": uri, "name": "polars"}],
            "capabilities": {
                "window": {"workDoneProgress": True},
                "textDocument": {
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                },
            },
            "initializationOptions": {
                "cachePriming": {"enable": True},
                "cargo": {"buildScripts": {"enable": True}},
            },
        }, timeout=300)
        self.notify("initialized", {})

    def wait_ready(self, settle: float = 8, max_wait: float = 1800):
        """Quiesce: indexing done when progress tokens stop for `settle`s."""
        deadline = time.time() + max_wait
        last_activity = time.time()
        self.proc.stdout.flush()
        import select
        while time.time() < deadline:
            r, _, _ = select.select([self.proc.stdout], [], [], 1.0)
            if r:
                self._track(self._recv())
                last_activity = time.time()
            elif (self.saw_progress and not self.active_progress
                    and time.time() - last_activity > settle):
                return
        raise TimeoutError("rust-analyzer never quiesced")

    def open_doc(self, rel: str) -> str:
        path = CRATES / rel
        uri = path.as_uri()
        self.notify("textDocument/didOpen", {"textDocument": {
            "uri": uri, "languageId": "rust", "version": 1,
            "text": path.read_text(errors="replace")}})
        return uri

    def references_for_file(self, rel: str, symbols: list) -> Counter:
        uri = self.open_doc(rel)
        doc_syms = self.request(
            "textDocument/documentSymbol", {"textDocument": {"uri": uri}})
        wanted = {}

        def walk(items):
            for it in items or []:
                if it.get("name") in symbols and it["name"] not in wanted:
                    wanted[it["name"]] = it["selectionRange"]["start"]
                walk(it.get("children"))
        walk(doc_syms)
        scores = Counter()
        for name, pos in wanted.items():
            try:
                refs = self.request("textDocument/references", {
                    "textDocument": {"uri": uri}, "position": pos,
                    "context": {"includeDeclaration": False}}, timeout=90)
            except (TimeoutError, RuntimeError):
                continue
            for ref in refs or []:
                p = ref["uri"]
                if not p.startswith("file://"):
                    continue
                fp = Path(p[7:])
                try:
                    frel = str(fp.relative_to(CRATES))
                except ValueError:
                    continue
                if frel != rel and frel.endswith(".rs"):
                    scores[frel] += 1
        self.notify("textDocument/didClose",
                    {"textDocument": {"uri": uri}})
        return scores


def run_lsp():
    trials = load_trials()
    client = LspClient(REPO)
    client.initialize()
    print("rust-analyzer indexing (this can take several minutes)...")
    t0 = time.time()
    client.wait_ready()
    print(f"ready in {time.time() - t0:.0f}s")
    rows, done = [], 0
    by_seed = {}
    for t in trials:
        if t["seed"] not in by_seed:
            by_seed[t["seed"]] = None
    for seed in by_seed:
        try:
            scores = client.references_for_file(seed, seed_symbols(seed))
        except Exception as e:  # keep going; empty ranking = honest miss
            print(f"  ! {seed}: {e}", file=sys.stderr)
            scores = Counter()
        by_seed[seed] = [f for f, _ in scores.most_common(TOP_K)]
        done += 1
        if done % 10 == 0:
            print(f"  {done}/{len(by_seed)} seeds")
    for t in trials:
        rows.append({"pr": t["pr"], "seed": t["seed"],
                     "ranked": by_seed[t["seed"]]})
    emit("lsp", rows)
    client.proc.terminate()


# ---------------------------------------------------------------- scoring

def score():
    trials = {(t["pr"], t["seed"]): t for t in load_trials()}
    arms = sorted(RESULTS.glob("blast_*.jsonl"))
    if not arms:
        sys.exit("no results; run arms first")
    print(f"{'arm':<12} {'bucket':<8} {'PRs':>4} {'trials':>6} "
          f"{'Recall@10':>10} {'MRR':>7} {'Hit@10':>7}")
    for path in arms:
        arm = path.stem.removeprefix("blast_")
        per_pr = defaultdict(list)
        for line in path.read_text().splitlines():
            r = json.loads(line)
            t = trials.get((r["pr"], r["seed"]))
            if t is None:
                continue
            gold = set(t["gold"])
            top10 = r["ranked"][:10]
            recall = len(gold & set(top10)) / len(gold)
            mrr = 0.0
            for i, f in enumerate(r["ranked"], 1):
                if f in gold:
                    mrr = 1.0 / i
                    break
            hit = 1.0 if gold & set(top10) else 0.0
            per_pr[(t["whale"], r["pr"])].append((recall, mrr, hit))
        for whale in (False, True):
            prs = {pr: v for (w, pr), v in per_pr.items() if w == whale}
            if not prs:
                continue
            n_trials = sum(len(v) for v in prs.values())
            # macro: mean over seeds within PR, then mean over PRs
            agg = [tuple(sum(x[i] for x in v) / len(v) for i in range(3))
                   for v in prs.values()]
            m = [sum(a[i] for a in agg) / len(agg) for i in range(3)]
            bucket = "whale" if whale else "primary"
            print(f"{arm:<12} {bucket:<8} {len(prs):>4} {n_trials:>6} "
                  f"{m[0]:>10.3f} {m[1]:>7.3f} {m[2]:>7.3f}")


# ---------------------------------------------------------------- main

if __name__ == "__main__":
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    {
        "gen": gen_trials,
        "roux": lambda: run_roux(1),
        "roux2": lambda: run_roux(2),
        "codegraph": lambda: run_codegraph(1),
        "codegraph2": lambda: run_codegraph(2),
        "gitnexus": run_gitnexus,
        "grep": lambda: run_grep(True),
        "grepraw": lambda: run_grep(False),
        "lsp": run_lsp,
        "score": score,
    }.get(cmd, lambda: sys.exit(__doc__))()
