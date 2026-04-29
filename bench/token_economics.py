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
from grep_vs_roux_personas import PQ, parse_personas  # noqa: E402

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


def _success(result_text: str, expected: list[str]) -> bool:
    if not result_text:
        return False
    lower = result_text.lower()
    return any(e.lower() in lower for e in expected)


def _preview(result_text: str, n: int = 240) -> str:
    if not result_text:
        return ""
    return result_text[:n].replace("\n", " ")


def drive_claude(
    pq: PQ, arm: str, run_idx: int, model: str
) -> RunResult:
    cfg = CONFIGS_DIR / "claude" / f"{arm}.json"
    prompt = PROMPT_TEMPLATE.format(query=pq.query)
    extra_tools: list[str] = []
    if arm == "with-roux":
        extra_tools = [
            "mcp__roux__roux_query",
            "mcp__roux__roux_list",
            "mcp__roux__roux_status",
        ]
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
    prompt = PROMPT_TEMPLATE.format(query=pq.query)
    cmd = [
        "codex", "exec",
        "--json",
        "--full-auto",  # workspace-write + low-friction approvals
        "--skip-git-repo-check",
        "--ephemeral",  # avoid session-persistence races across rapid runs
        # Pin reasoning effort for reproducibility / cost control.
        "-c", "model_reasoning_effort=\"medium\"",
    ]
    if model:
        cmd += ["--model", model]
    if arm == "with-roux":
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
    prompt = PROMPT_TEMPLATE.format(query=pq.query)
    # Use custom agent profiles instead of --enabled-tools (which made
    # Devstral 2 hallucinate tool calls as text content). Profiles live at
    # ~/.vibe/agents/roux-bench-{with,without}.toml and toggle which MCP
    # tools are exposed; setup is one-shot per machine — see the harness
    # docstring or docs/token-economics.md for the file contents.
    agent = "roux-bench-with" if arm == "with-roux" else "roux-bench-without"
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
        return RunResult(
            agent=agent, model=model, arm=arm,
            persona=short_persona(pq.persona), query=pq.query,
            expected=list(pq.expected), run_idx=run_idx, run_id=run_id,
            wall_ms=wall_ms,
            **parsed,
            success=_success(parsed["result_preview"], list(pq.expected)),
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
    }


VIBE_SESSIONS_DIR = Path.home() / ".vibe" / "logs" / "session"


def _final_assistant_text_from_messages(arr: list[dict]) -> str:
    """Pull the last *terminal* assistant message — one whose tool_calls is
    empty — so we grade real answers, not the agent's narration of tool calls
    embedded in content."""
    for msg in reversed(arr):
        if not isinstance(msg, dict) or msg.get("role") != "assistant":
            continue
        if msg.get("tool_calls"):
            continue  # not terminal — agent is still calling tools
        t = msg.get("content") or ""
        if t:
            return t
    # Fallback: last assistant of any kind (may include hallucinated content,
    # but at least gives the success metric something to grade against).
    for msg in reversed(arr):
        if isinstance(msg, dict) and msg.get("role") == "assistant":
            t = msg.get("content") or ""
            if t:
                return t
    return ""


def parse_vibe(stdout: str, stderr: str) -> dict[str, Any]:
    """Vibe's `--output json` mode emits a messages array (no token stats),
    but writes per-session metadata with token + cost stats to
    ~/.vibe/logs/session/<id>/meta.json. Pick the session dir created during
    this run (most-recent start_time) and read stats from it."""
    final_text = ""
    try:
        data = json.loads(stdout)
        if isinstance(data, list):
            final_text = _final_assistant_text_from_messages(data)
    except json.JSONDecodeError:
        # streaming mode emits one message-JSON per line
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

    # Grab the freshest meta.json (sessions are written sequentially).
    stats: dict[str, Any] = {}
    if VIBE_SESSIONS_DIR.exists():
        sessions = sorted(
            (p for p in VIBE_SESSIONS_DIR.iterdir() if p.is_dir()),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
        for sd in sessions[:3]:  # only check the few most recent
            meta = sd / "meta.json"
            if meta.exists():
                try:
                    m = json.loads(meta.read_text())
                    stats = m.get("stats", {}) or {}
                    break
                except (json.JSONDecodeError, OSError):
                    continue

    return {
        "input_tokens": int(stats.get("session_prompt_tokens", 0) or 0),
        "output_tokens": int(stats.get("session_completion_tokens", 0) or 0),
        "cache_read_tokens": 0,  # Vibe stats don't separately report cache hits
        "cache_creation_tokens": 0,
        "reasoning_output_tokens": 0,
        "num_turns": int(stats.get("steps", 0) or 0),
        "result_preview": _preview(final_text or stdout[-400:]),
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

    total = len(queries) * len(only_agents) * 2 * runs_per_task
    done = 0
    print(f"[harness] {len(queries)} queries × {len(only_agents)} agents × 2 arms × {runs_per_task} runs = {total} runs → {out_path}", file=sys.stderr)
    if skipped_agents:
        print(f"[harness] skipped: {','.join(skipped_agents)}", file=sys.stderr)

    with out_path.open("w") as fh:
        for q in queries:
            for agent in sorted(only_agents):
                drive, model_fn = DRIVERS[agent]
                model = model_fn()
                for arm in ("no-roux", "with-roux"):
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
