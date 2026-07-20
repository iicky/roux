#!/usr/bin/env python3
"""Cross-agent token economics harness for roux.

Drives Claude Code, Codex CLI, and Mistral Vibe in non-interactive mode against
the persona query set with two arms (no-roux baseline + with-roux MCP). Captures
input/output/cache tokens, wall-clock, success-vs-expected per task. Emits a
JSONL stream of results plus a summary markdown table.

Reproducibility: re-run with `bench/token_economics.py` against a checked-out
roux. Persona repos auto-clone to /tmp/roux-sources/ on first run; per-persona
roux indexes auto-build via `roux init --local`.

One-time per-machine setup:
  - Codex CLI authenticated (`codex login`).
  - Claude Code authenticated (any of: subscription, API key, Bedrock).
  - Vibe authenticated (`vibe --setup`) AND the harness's two agent profiles
    installed: `cp bench/agent-configs/vibe/roux-bench-*.toml ~/.vibe/agents/`.
    The roux MCP server must also be registered in ~/.vibe/config.toml — see
    bench/agent-configs/vibe/README.md (or just paste an inline-table mcp_servers
    entry pointing to the roux binary's `serve --local` mode).

Env knobs:
  ONLY_AGENTS=claude,codex,vibe   # comma-separated subset
  ONLY_PERSONAS=ripgrep,pandas    # comma-separated persona names
  ONLY_QUERIES=42                 # numeric: take first N queries (for smoke tests)
  RUNS_PER_TASK=3                 # repetitions per (agent, arm, query) tuple
  CLAUDE_MODEL / CODEX_MODEL / VIBE_MODEL
  ROUX_BIN=/path/to/roux          # default: target/release/roux

Cost: full matrix (40q × 3 agents × 2 arms × 3 runs ≈ 720 task runs) lands
around $60-80 across providers. Use ONLY_* knobs to scope smoke tests cheaper.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import time
import uuid
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(REPO_ROOT / "bench"))
from persona_query_set import PQ, parse_personas  # noqa: E402

CONFIGS_DIR = REPO_ROOT / "bench" / "agent-configs"
RESULTS_DIR = REPO_ROOT / "bench" / "results"
SOURCES_DIR = Path(os.environ.get("ROUX_SOURCES", "/tmp/roux-sources"))
ROUX_BIN = Path(os.environ.get("ROUX_BIN", REPO_ROOT / "target" / "release" / "roux"))

PERSONA_GIT = {
    # name → (branch, url) — matches .github/workflows/persona-bench.yml
    "ripgrep": ("14.1.1", "https://github.com/BurntSushi/ripgrep"),
    "pandas": ("v2.2.3", "https://github.com/pandas-dev/pandas"),
    "remix": ("remix@2.13.1", "https://github.com/remix-run/remix"),
    "gin": ("v1.10.0", "https://github.com/gin-gonic/gin"),
    "Marlin": ("2.1.2.5", "https://github.com/MarlinFirmware/Marlin"),
}

# Persona-name aliases (rust file uses fancy names like "rust-cli (ripgrep)";
# we want the short tag for cloning + filtering).
def short_persona(name: str) -> str:
    if "ripgrep" in name:
        return "ripgrep"
    if "pandas" in name:
        return "pandas"
    if "remix" in name:
        return "remix"
    if "gin" in name:
        return "gin"
    if "Marlin" in name or "marlin" in name.lower():
        return "Marlin"
    return name


@dataclass
class RunResult:
    agent: str
    model: str
    arm: str  # "no-roux" | "with-roux"
    persona: str
    query: str
    expected: list[str]
    run_idx: int
    run_id: str
    wall_ms: int
    input_tokens: int
    output_tokens: int
    cache_read_tokens: int
    cache_creation_tokens: int
    reasoning_output_tokens: int
    num_turns: int
    success: bool
    result_preview: str
    error: str | None


# ─── Setup ─────────────────────────────────────────────────────────────

def ensure_persona(short_name: str, path: Path) -> bool:
    """Clone the persona repo + build a roux index. Returns False on failure."""
    if not path.exists() or not (path / ".git").exists():
        if short_name not in PERSONA_GIT:
            print(f"[setup] no clone info for {short_name}; skipping", file=sys.stderr)
            return False
        branch, url = PERSONA_GIT[short_name]
        print(f"[setup] cloning {short_name} → {path}", file=sys.stderr)
        path.parent.mkdir(parents=True, exist_ok=True)
        rc = subprocess.call(
            ["git", "clone", "--depth=1", f"--branch={branch}", url, str(path)]
        )
        if rc != 0:
            print(f"[setup] clone failed for {short_name}", file=sys.stderr)
            return False

    db = path / ".roux" / "db.sqlite"
    if not db.exists():
        print(f"[setup] roux init {short_name}", file=sys.stderr)
        rc = subprocess.call(
            [str(ROUX_BIN), "init", "--local"], cwd=str(path)
        )
        if rc != 0:
            print(f"[setup] roux init failed for {short_name} (with-roux arm will skip)", file=sys.stderr)
            return False
    return True


# ─── Drivers ───────────────────────────────────────────────────────────

PROMPT_TEMPLATE = (
    "Find and explain how this codebase implements: {query}. "
    "Cite file:line for the key symbols you find. Be concise — under 200 words."
)

# roux-first arm: same task, but instruct the agent to treat roux as the primary
# retrieval surface and avoid re-reading files it already found via roux. Tests
# whether the with-roux cost penalty is behavioral (over-reading) vs structural
# (MCP schema + output overhead).
PROMPT_TEMPLATE_ROUX_FIRST = (
    "Find and explain how this codebase implements: {query}. "
    "Use the roux_query tool as your PRIMARY way to locate code — it returns "
    "symbols with file:line, signatures, docs, and their caller/callee neighborhood. "
    "Rely on roux's output; only open a file with Read/Grep if roux's results are "
    "genuinely insufficient to answer. Cite file:line. Be concise — under 200 words."
)


# context-prep arm: roux runs ONCE up front; a compact skeleton of the
# ranked symbols is injected into the prompt prefix, and the agent gets NO live
# roux tool. Tests roux-as-preprocessor — removes the per-turn schema tax and
# the turn-amplification, and the injected block is a cacheable prefix.
PROMPT_TEMPLATE_CONTEXT_PREP = (
    "A code-retrieval tool (roux) pre-located the most relevant symbols for this "
    "task — ranked, with file:line, signatures, and docs:\n\n{context}\n\n"
    "Using this as your starting point, find and explain how this codebase "
    "implements: {query}. Cite file:line. Open files with Read/Grep only if the "
    "context above is insufficient. Be concise — under 200 words."
)


# context-prep-bodies: substitutive output. Inject the actual source
# body of the top-K ranked symbols so the agent can answer WITHOUT re-reading
# files. Bounded by env to keep the injected block from blowing up token count:
#   PREP_BODY_K     = how many top symbols get bodies (default 3)
#   PREP_BODY_LINES = source lines per body window (default 30)
PREP_BODY_K = int(os.environ.get("PREP_BODY_K", "3"))
PREP_BODY_LINES = int(os.environ.get("PREP_BODY_LINES", "30"))


def _read_span(repo: Path, file_rel: str, start_line: int, n: int) -> str:
    try:
        text = (repo / file_rel).read_text(errors="replace")
    except OSError:
        return ""
    src = text.splitlines()
    s = max(0, int(start_line) - 1)
    return "\n".join(src[s:s + n])


def _neighbor_names(target: dict, symbols: list[dict], edges: list[dict],
                    id_to_qn: dict[str, str]) -> list[str]:
    """1-hop neighbor NAMES for the --neighbors layer (names only, no bodies).
    Best-effort from CLI JSON: parent (parent_id), children (symbols whose parent
    is this one), and edge peers. Neighbors whose id is not in the result set are
    unresolvable from CLI JSON (the graph has the name; the export doesn't) — those
    are skipped. The native roux --format skeleton --neighbors would resolve all."""
    tid = target.get("id")
    names: list[str] = []
    seen = {tid}
    pid = target.get("parent_id")
    if pid and pid in id_to_qn and pid not in seen:
        names.append(id_to_qn[pid]); seen.add(pid)
    for s in symbols:
        sid = s.get("id")
        if s.get("parent_id") == tid and sid not in seen:
            names.append(id_to_qn.get(sid, s.get("name", ""))); seen.add(sid)
    for e in edges:
        peer = e.get("to") if e.get("from") == tid else (e.get("from") if e.get("to") == tid else None)
        if peer and peer in id_to_qn and peer not in seen:
            names.append(id_to_qn[peer]); seen.add(peer)
    return [n.split("::")[-1] for n in names if n]


def roux_context(pq: PQ, bodies: bool = False, neighbors: bool = False,
                 scores: bool = False) -> str:
    """Run roux once and render a COMPACT skeleton block (drop the hash-laden
    edges/matched arrays the raw JSON carries; keep name/loc/sig/doc). Layers:
    bodies=True inlines top-PREP_BODY_K source spans (disproven — kept for
    reference). neighbors=True adds a names-only `near:` line (Layer 1).
    scores=True prefixes each entry with roux's raw score (Layer 2 — raw,
    UNCALIBRATED, the test is whether even raw scores shift agent behavior)."""
    try:
        proc = subprocess.run(
            [str(ROUX_BIN), "query", pq.query, "--local", "--format", "json", "--top", "5"],
            cwd=str(pq.path), capture_output=True, text=True, timeout=60, check=False,
        )
        data = json.loads(proc.stdout)
    except (json.JSONDecodeError, OSError, subprocess.SubprocessError):
        return "(roux returned no usable context)"
    symbols = data.get("symbols", [])
    edges = data.get("edges", [])
    id_to_qn = {s.get("id"): (s.get("qualified_name") or s.get("name", "")) for s in symbols}
    lines = []
    for idx, s in enumerate(symbols):
        qn = s.get("qualified_name") or s.get("name", "")
        loc = f'{s.get("file", "?")}:{s.get("line", "?")}'
        sig = (s.get("signature") or "").strip()
        doc = (s.get("doc") or "").replace("\n", " ").strip()
        if len(doc) > 160:
            doc = doc[:160] + "…"
        prefix = f"[{s.get('score', 0):.2f}] " if scores else ""
        entry = f"- {prefix}{qn} ({loc})"
        if sig:
            entry += f"\n    {sig}"
        if doc:
            entry += f"\n    // {doc}"
        if neighbors:
            near = _neighbor_names(s, symbols, edges, id_to_qn)
            if near:
                entry += f"\n    near: {', '.join(near[:5])}"
        if bodies and idx < PREP_BODY_K:
            span = _read_span(Path(pq.path), s.get("file", ""), s.get("line", 1), PREP_BODY_LINES)
            if span:
                entry += f"\n  ```\n{span}\n  ```"
        lines.append(entry)
    return "\n".join(lines) if lines else "(roux found no symbols)"


def roux_format_cli(pq: PQ, fmt: str, top: int = 5) -> str:
    """Render context via a SHIPPED `roux query --format <fmt>` primitive
    verbatim (fmt = skeleton | compact), so the measured block is byte-for-byte
    what production emits — the skeleton bytes match the `roux://skeleton/{query}`
    MCP resource; the compact bytes match roux_query's default output — instead of
    a Python re-implementation that can silently drift from it."""
    try:
        proc = subprocess.run(
            [str(ROUX_BIN), "query", pq.query, "--local",
             "--format", fmt, "--top", str(top)],
            cwd=str(pq.path), capture_output=True, text=True, timeout=60, check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return "(roux returned no usable context)"
    out = proc.stdout.strip()
    return out if out else "(roux found no symbols)"


def build_prompt(arm: str, pq: PQ) -> str:
    # Arms that inject a shipped roux output block as the prompt prefix.
    cli_fmt = {"context-prep": "skeleton", "context-prep-compact": "compact"}
    if arm in cli_fmt:
        ctx = roux_format_cli(pq, cli_fmt[arm])
        return PROMPT_TEMPLATE_CONTEXT_PREP.format(context=ctx, query=pq.query)
    # Layered variants need fields the skeleton format omits (bodies/neighbors/
    # scores), so they stay on the JSON renderer.
    layered = {
        "context-prep-bodies": dict(bodies=True),
        "context-prep-neighbors": dict(neighbors=True),
        "context-prep-scores": dict(scores=True),
    }
    if arm in layered:
        ctx = roux_context(pq, **layered[arm])
        return PROMPT_TEMPLATE_CONTEXT_PREP.format(context=ctx, query=pq.query)
    tmpl = PROMPT_TEMPLATE_ROUX_FIRST if arm == "roux-first" else PROMPT_TEMPLATE
    return tmpl.format(query=pq.query)


def _roux_enabled(arm: str) -> bool:
    # context-prep injects roux output statically — no live MCP tool.
    return arm in ("with-roux", "roux-first")


# A concrete source location the agent actually pointed at: file.ext:line.
_CITATION_RE = re.compile(r"[\w./-]+\.[A-Za-z][A-Za-z0-9+]*:\d+")


def _success(final_text: str, expected: list[str]) -> bool:
    """Grade the FULL agent answer (not a truncated preview) against the gold.

    A hit requires the answer to BOTH name a gold symbol AND cite a concrete
    file:line location. The citation requirement is what stops a disclaimer that
    merely echoes the symbol name — "I could not find SearcherBuilder" — from
    grading as SUCCESS, and makes the pass/fail signal publicly citable rather
    than substring-lucky."""
    if not final_text:
        return False
    lower = final_text.lower()
    if not any(e.lower() in lower for e in expected):
        return False
    return bool(_CITATION_RE.search(final_text))


def _preview(result_text: str, n: int = 240) -> str:
    if not result_text:
        return ""
    return result_text[:n].replace("\n", " ")


def drive_claude(
    pq: PQ, arm: str, run_idx: int, model: str
) -> RunResult:
    cfg = CONFIGS_DIR / "claude" / ("with-roux.json" if _roux_enabled(arm) else "no-roux.json")
    prompt = build_prompt(arm, pq)
    extra_tools: list[str] = []
    if _roux_enabled(arm):
        extra_tools = ["mcp__roux__roux_query"]
    cmd = [
        "claude", "-p", prompt,
        "--output-format", "json",
        "--strict-mcp-config",
        "--mcp-config", str(cfg),
        "--allowedTools", "Read", "Grep", "Glob", "Bash", *extra_tools,
        "--max-turns", "15",
        "--model", model,
    ]
    return _run_subprocess("claude", model, arm, pq, run_idx, cmd, str(pq.path), parse_claude)


def drive_codex(
    pq: PQ, arm: str, run_idx: int, model: str
) -> RunResult:
    prompt = build_prompt(arm, pq)
    cmd = [
        "codex", "exec",
        "--json",
        # codex 0.139 dropped --full-auto. The task is read+explain only, so a
        # read-only sandbox is the safest match; never prompt (non-interactive).
        "--sandbox", "read-only",
        "-c", "approval_policy=\"never\"",
        "--skip-git-repo-check",
        "--ephemeral",  # avoid session-persistence races across rapid runs
        # Pin reasoning effort for reproducibility / cost control.
        "-c", "model_reasoning_effort=\"medium\"",
    ]
    if model:
        cmd += ["--model", model]
    if _roux_enabled(arm):
        cmd += [
            "-c", f'mcp_servers.roux.command="{ROUX_BIN}"',
            "-c", 'mcp_servers.roux.args=["serve","--local"]',
        ]
    cmd.append(prompt)
    return _run_subprocess("codex", model, arm, pq, run_idx, cmd, str(pq.path), parse_codex)


def drive_vibe(
    pq: PQ, arm: str, run_idx: int, model: str
) -> RunResult:
    """Vibe has no per-invocation config-file override — MCP servers must be
    registered globally in ~/.vibe/config.toml. The harness toggles arms via
    `--enabled-tools`: no-roux gets only the file-system tool set, with-roux
    additionally allows the MCP roux tool names (which only resolve if the
    user has registered the roux MCP server in their global config — see
    docs/token-economics.md for setup)."""
    prompt = build_prompt(arm, pq)
    # Use custom agent profiles instead of --enabled-tools (which made
    # Devstral 2 hallucinate tool calls as text content). Profiles live at
    # ~/.vibe/agents/roux-bench-{with,without}.toml and toggle which MCP
    # tools are exposed; setup is one-shot per machine — see the harness
    # docstring or docs/token-economics.md for the file contents.
    agent = "roux-bench-with" if _roux_enabled(arm) else "roux-bench-without"
    cmd = [
        "vibe",
        "--prompt", prompt,
        "--max-turns", "15",
        "--max-price", "0.50",
        "--output", "json",
        "--agent", agent,
    ]
    return _run_subprocess("vibe", model, arm, pq, run_idx, cmd, str(pq.path), parse_vibe)


def _run_subprocess(
    agent: str, model: str, arm: str, pq: PQ, run_idx: int,
    cmd: list[str], cwd: str, parse_fn,
) -> RunResult:
    run_id = f"{agent}-{arm}-{uuid.uuid4().hex[:8]}"
    start = time.time()
    try:
        proc = subprocess.run(
            cmd, cwd=cwd, capture_output=True, text=True, timeout=300, check=False,
            stdin=subprocess.DEVNULL,
        )
        wall_ms = int((time.time() - start) * 1000)
        if proc.returncode != 0:
            return _empty_result(
                agent, model, arm, pq, run_idx, run_id, wall_ms,
                error=f"exit={proc.returncode}: {proc.stderr[:200]}",
            )
        parsed = parse_fn(proc.stdout, proc.stderr)
        final_text = parsed.pop("final_text", parsed.get("result_preview", ""))
        return RunResult(
            agent=agent, model=model, arm=arm,
            persona=short_persona(pq.persona), query=pq.query,
            expected=list(pq.expected), run_idx=run_idx, run_id=run_id,
            wall_ms=wall_ms,
            **parsed,
            success=_success(final_text, list(pq.expected)),
            error=None,
        )
    except subprocess.TimeoutExpired:
        wall_ms = int((time.time() - start) * 1000)
        return _empty_result(
            agent, model, arm, pq, run_idx, run_id, wall_ms,
            error="timeout=300s",
        )


def _empty_result(
    agent: str, model: str, arm: str, pq: PQ, run_idx: int,
    run_id: str, wall_ms: int, error: str,
) -> RunResult:
    return RunResult(
        agent=agent, model=model, arm=arm,
        persona=short_persona(pq.persona), query=pq.query,
        expected=list(pq.expected), run_idx=run_idx, run_id=run_id,
        wall_ms=wall_ms,
        input_tokens=0, output_tokens=0, cache_read_tokens=0,
        cache_creation_tokens=0, reasoning_output_tokens=0, num_turns=0,
        success=False, result_preview="", error=error,
    )


# ─── Per-agent JSON parsers ────────────────────────────────────────────

def parse_claude(stdout: str, stderr: str) -> dict[str, Any]:
    data = json.loads(stdout)
    usage = data.get("usage", {}) or {}
    return {
        "input_tokens": int(usage.get("input_tokens", 0) or 0),
        "output_tokens": int(usage.get("output_tokens", 0) or 0),
        "cache_read_tokens": int(usage.get("cache_read_input_tokens", 0) or 0),
        "cache_creation_tokens": int(usage.get("cache_creation_input_tokens", 0) or 0),
        "reasoning_output_tokens": 0,
        "num_turns": int(data.get("num_turns", 0) or 0),
        "result_preview": _preview(data.get("result", "") or ""),
        "final_text": data.get("result", "") or "",
    }


def parse_codex(stdout: str, stderr: str) -> dict[str, Any]:
    """Codex emits JSONL events; the terminal turn.completed carries usage."""
    last_usage: dict[str, Any] = {}
    final_text = ""
    num_turns = 0
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "turn.completed":
            num_turns += 1
            if isinstance(ev.get("usage"), dict):
                last_usage = ev["usage"]
        # Capture last assistant message text from item.completed events.
        # Codex shape: {"type":"item.completed","item":{"id":"item_N","type":"agent_message","text":"..."}}
        if ev.get("type") == "item.completed" and isinstance(ev.get("item"), dict):
            item = ev["item"]
            if item.get("type") in ("agent_message", "message") or item.get("kind") == "agent_message":
                t = item.get("text") or item.get("content") or ""
                if t:
                    final_text = t
    return {
        "input_tokens": int(last_usage.get("input_tokens", 0) or 0),
        "output_tokens": int(last_usage.get("output_tokens", 0) or 0),
        "cache_read_tokens": int(last_usage.get("cached_input_tokens", 0) or 0),
        "cache_creation_tokens": 0,
        "reasoning_output_tokens": int(last_usage.get("reasoning_output_tokens", 0) or 0),
        "num_turns": num_turns,
        "result_preview": _preview(final_text),
        "final_text": final_text,
    }


VIBE_SESSIONS_DIR = Path.home() / ".vibe" / "logs" / "session"

# Devstral 2 sometimes emits tool calls as literal text content instead of
# structured tool_calls (e.g. `read_file{"path": ...} grep{...}`). Such a
# message is the agent failing to act, not a real answer — never grade it.
_TOOL_NARRATION_RE = re.compile(
    r'^\s*(read_file|grep|list_files|write_file|run_command|glob|bash|roux_\w+)\s*\{'
)


def _looks_like_tool_narration(text: str) -> bool:
    return bool(_TOOL_NARRATION_RE.match(text or ""))


def _final_assistant_text_from_messages(arr: list[dict]) -> str:
    """Last *terminal* assistant message — tool_calls empty AND not tool-call
    narration-as-text — so we grade real answers, not the agent's narration."""
    for msg in reversed(arr):
        if not isinstance(msg, dict) or msg.get("role") != "assistant":
            continue
        if msg.get("tool_calls"):
            continue  # not terminal — agent is still calling tools
        t = msg.get("content") or ""
        if t and not _looks_like_tool_narration(t):
            return t
    return ""


def _freshest_vibe_session() -> Path | None:
    if not VIBE_SESSIONS_DIR.exists():
        return None
    dirs = sorted(
        (p for p in VIBE_SESSIONS_DIR.iterdir() if p.is_dir()),
        key=lambda p: p.stat().st_mtime, reverse=True,
    )
    return dirs[0] if dirs else None


def _read_messages_jsonl(path: Path) -> list[dict]:
    msgs: list[dict] = []
    try:
        for line in path.read_text().splitlines():
            line = line.strip()
            if line:
                try:
                    msgs.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError:
        pass
    return msgs


def parse_vibe(stdout: str, stderr: str) -> dict[str, Any]:
    """Read BOTH the final answer and the token stats from the SAME session dir
    (~/.vibe/logs/session/<id>/{messages.jsonl,meta.json}) so they cannot
    mismatch across rapid sequential runs. stdout is only a fallback when the
    session files are unavailable."""
    sess = _freshest_vibe_session()
    stats: dict[str, Any] = {}
    final_text = ""
    if sess is not None:
        meta = sess / "meta.json"
        if meta.exists():
            try:
                stats = json.loads(meta.read_text()).get("stats") or {}
            except (json.JSONDecodeError, OSError):
                stats = {}
        final_text = _final_assistant_text_from_messages(
            _read_messages_jsonl(sess / "messages.jsonl")
        )

    if not final_text:
        # Fallback: parse stdout (--output json array, or streamed JSON lines).
        try:
            data = json.loads(stdout)
            if isinstance(data, list):
                final_text = _final_assistant_text_from_messages(data)
        except json.JSONDecodeError:
            msgs: list[dict] = []
            for line in stdout.splitlines():
                line = line.strip()
                if not line:
                    continue
                try:
                    msgs.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
            final_text = _final_assistant_text_from_messages(msgs)

    return {
        "input_tokens": int(stats.get("session_prompt_tokens", 0) or 0),
        "output_tokens": int(stats.get("session_completion_tokens", 0) or 0),
        "cache_read_tokens": 0,  # Vibe stats don't separately report cache hits
        "cache_creation_tokens": 0,
        "reasoning_output_tokens": 0,
        "num_turns": int(stats.get("steps", 0) or 0),
        "result_preview": _preview(final_text),
        "final_text": final_text,
    }


# ─── Main ──────────────────────────────────────────────────────────────

DRIVERS = {
    "claude": (drive_claude, lambda: os.environ.get("CLAUDE_MODEL", "claude-sonnet-4-6")),
    "codex": (drive_codex, lambda: os.environ.get("CODEX_MODEL", "")),  # "" = use config default
    "vibe": (drive_vibe, lambda: os.environ.get("VIBE_MODEL", "devstral-2")),
}


def cli_available(name: str) -> bool:
    return shutil.which(name) is not None


def main() -> int:
    only_agents = set(filter(None, os.environ.get("ONLY_AGENTS", "claude,codex,vibe").split(",")))
    only_personas = set(filter(None, os.environ.get("ONLY_PERSONAS", "").split(",")))
    only_queries_n = int(os.environ.get("ONLY_QUERIES", "0") or 0)
    runs_per_task = int(os.environ.get("RUNS_PER_TASK", "3"))
    arms = tuple(filter(None, os.environ.get("ARMS", "no-roux,with-roux").split(",")))

    queries = parse_personas()
    if only_personas:
        queries = [q for q in queries if short_persona(q.persona) in only_personas]
    if only_queries_n:
        queries = queries[:only_queries_n]
    if not queries:
        print("no queries to run after filtering", file=sys.stderr)
        return 1

    # Setup phase: clone + index needed personas
    needed = {short_persona(q.persona): Path(q.path) for q in queries}
    for short, path in needed.items():
        ensure_persona(short, path)

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    ts = time.strftime("%Y%m%dT%H%M")
    out_path = RESULTS_DIR / f"token_economics_{ts}.jsonl"

    skipped_agents = []
    for agent in list(only_agents):
        if not cli_available(agent):
            print(f"[skip] {agent} not on PATH", file=sys.stderr)
            skipped_agents.append(agent)
            only_agents.discard(agent)
    if not only_agents:
        print("no agents available", file=sys.stderr)
        return 1

    total = len(queries) * len(only_agents) * len(arms) * runs_per_task
    done = 0
    print(f"[harness] {len(queries)} queries × {len(only_agents)} agents × {len(arms)} arms ({','.join(arms)}) × {runs_per_task} runs = {total} runs → {out_path}", file=sys.stderr)
    if skipped_agents:
        print(f"[harness] skipped: {','.join(skipped_agents)}", file=sys.stderr)

    with out_path.open("w") as fh:
        for q in queries:
            for agent in sorted(only_agents):
                drive, model_fn = DRIVERS[agent]
                model = model_fn()
                for arm in arms:
                    for run_idx in range(1, runs_per_task + 1):
                        done += 1
                        print(
                            f"[{done}/{total}] {agent} {arm} {short_persona(q.persona)}/{q.query[:40]}... run={run_idx}",
                            file=sys.stderr,
                        )
                        result = drive(q, arm, run_idx, model)
                        fh.write(json.dumps(asdict(result)) + "\n")
                        fh.flush()
    print(f"[harness] done → {out_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
