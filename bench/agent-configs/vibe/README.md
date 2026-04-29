# Vibe agent profiles for the token-economics harness

Vibe (Mistral) doesn't accept a per-invocation config-file override; tools live
in `~/.vibe/config.toml` globally. The harness routes around that by using two
custom agent profiles installed at `~/.vibe/agents/`:

- `roux-bench-with.toml` — keeps the roux MCP tools (`roux_roux_*`) enabled
- `roux-bench-without.toml` — disables them so the no-roux arm sees only file tools

## One-time setup on a fresh machine

1. Install both agent profiles:

   ```sh
   cp bench/agent-configs/vibe/roux-bench-with.toml    ~/.vibe/agents/
   cp bench/agent-configs/vibe/roux-bench-without.toml ~/.vibe/agents/
   ```

2. Register the roux MCP server in `~/.vibe/config.toml`. Replace the existing
   `mcp_servers = []` line with:

   ```toml
   mcp_servers = [
     { name = "roux", transport = "stdio",
       command = "/absolute/path/to/target/release/roux",
       args = ["serve", "--local"],
       startup_timeout_sec = 15, tool_timeout_sec = 60 },
   ]
   ```

   (Inline-table syntax — Vibe's `mcp_servers = []` declaration in the default
   config locks the key as a flat array, so `[[mcp_servers]]` array-of-tables
   syntax fails to parse.)

3. Sanity-check: `vibe --version` should still work, and
   `vibe --prompt "List your tools" --output text --agent roux-bench-with`
   should mention `roux_roux_query`, `roux_roux_list`, and `roux_roux_status`
   among the available tools.

## Why custom agents instead of `--enabled-tools`

Vibe's `--enabled-tools` flag is exclusive — passing it disables every tool not
listed. In practice, that mode caused Devstral 2 to hallucinate tool-call JSON
as plain assistant text instead of actually invoking tools. Custom agent
profiles (which set `disabled_tools` rather than `enabled_tools`) leave the
default tool surface intact and only gate the roux MCP namespace, which sidesteps
the hallucination.
