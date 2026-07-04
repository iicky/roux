# Token economics

Internal measurement log for roux-b3v: does roux cost or save agent API spend,
and how does that depend on *how* roux is delivered to the agent? These are
preliminary N=1 results on a single repo — directional signal that replaces the
back-of-napkin estimate, **not** a claim to quote. No headline numbers here until
there's a product and the runs to back them (N≥3, multiple repos). See caveats.

Direction observed so far: roux as a *live per-turn MCP tool* trends toward a net
cost; roux as *one-shot cached context-prep* (run once, skeleton injected into the
prompt prefix) trends toward a cost reduction on a strong agent without a success
penalty. The delivery mechanism appears to decide the sign. Magnitudes below are
run-noisy — treat them as "which direction," not "how much."

## Method

`bench/token_economics.py` drives an agent (Codex CLI `gpt-5-codex`, Mistral Vibe
`devstral-2`) non-interactively over the persona query set, one arm per
configuration, and records `input/output/cache` tokens, wall-clock, turn count,
and success-vs-expected per task to JSONL under `bench/results/`. Pricing is
public list price as of the run date (`PRICING` in
`bench/aggregate_token_economics.py`): Codex $1.25/$10.00/M in/out, cache-read
$0.125/M; Vibe $0.20/$0.60/M.

Arms:

- **no-roux** — baseline. Agent answers with its native tools (ripgrep, file reads).
- **with-roux** — roux exposed as a live MCP tool the agent calls per turn.
- **roux-first** — same, but the prompt nudges roux before grep.
- **context-prep** — roux runs *once* up front; `roux query --format skeleton`
  output is injected statically into the prompt prefix. No live MCP. (This is the
  roux-iyi reframe.)
- **context-prep-compact** — same injection, but `roux query --format compact`:
  ranked matched symbols with signature, one-line doc, and neighbor NAMES under a
  token budget with an `(… N more)` marker. Measured ~22% of the JSON payload's
  bytes on ripgrep. (This is the roux-ufyq mode, also served by the live tool as
  `roux_query(compact=true)`.)
- **context-prep-bodies / -neighbors / -scores / -compressed** — variants of the
  injected block (full source bodies, graph neighbors, PPR scores, compressed).

All numbers below are Codex on the `ripgrep` persona, 8 queries, N=1 per cell
unless noted. See caveats — this is directional signal, not a confidence interval.

## Observation 1 — live MCP roux trended toward a net cost

`token_economics_20260621T2004.jsonl`, $/query:

| arm | Codex $/q | Codex turns/q | Vibe $/q | Vibe turns/q |
|---|---|---|---|---|
| no-roux | $0.181 | 1.0 | $0.0102 | 4.6 |
| roux-first | $0.195 (+7%) | 1.0 | $0.0222 (+118%) | 8.9 |
| with-roux | $0.215 (+19%) | 1.0 | $0.0159 (+56%) | 6.5 |

Two candidate mechanisms: a per-turn schema tax (the tool definition rides in
context every turn) and turn amplification — exposing the tool made the agent take
more turns (Vibe 4.6 → 8.9). Success was unchanged (Codex 7/8 across all arms), so
the extra spend bought nothing here. Note the Vibe percentages are large but the
absolute dollars are pennies ($0.010 → $0.022/q); don't quote the percentage
without the magnitude.

## Observation 2 — one-shot cached context-prep trended the other way

Run roux once, render a compact ranked skeleton, inject it into the
prompt prefix (prompt-cacheable: ~1.25× to write once, ~0.1× cache-read every
subsequent turn). $/query and success, three independent runs:

| run | no-roux $/q (ok) | context-prep $/q (ok) | Δ$ |
|---|---|---|---|
| 0622T1446 | $0.221 (6/8) | $0.138 (6/8) | −38% |
| 0622T1803 | $0.237 (6/8) | $0.138 (7/8) | −42% |
| 0622T1958 | $0.201 (6/8) | $0.169 (7/8) | −16% |

Direction was consistent (context-prep cheaper in all three runs) but the spread
(−16% to −42% on the same arm) is run noise on N=1, not a range of conditions —
don't read it as a point estimate. Success moved by at most one query (6/8↔7/8),
which is within noise; the honest read is "no success penalty," not "improved."
This clears the roux-iyi acceptance bar (context-prep input cost below baseline,
success not hurt). The skeleton primitive shipped as
`roux query --format skeleton` (commit 9a226f0), and is also exposed over MCP as
the `roux://skeleton/{query}` **resource** (not a tool) — clients read it once at
task start and inject it into the prompt prefix, so it prompt-caches and avoids
the per-turn tool-schema tax that made live MCP roux a net cost (Observation 1).

## Observation 3 — richer injection didn't beat plain skeleton

Variants of the injected block, same harness:

| variant | vs plain skeleton |
|---|---|
| bodies (full source) | ~equal-to-worse ($0.139–0.166 vs $0.138) — no benefit, more tokens |
| compressed | worse ($0.166) |
| neighbors | cheaper one run but dropped a success (6/8) |
| scores | comparable ($0.149, 7/8) |

Plain skeleton is the most consistent win. Bodies, compression, neighbors, and
scores are tested and not pursued.

## Caveats — what this is *not*

- **N=1, 8 queries, single repo (ripgrep).** Directional, not statistically
  powered. The 16–42% spread on the same arm shows per-run variance is large.
- **The win is agent-strength-dependent.** On Vibe (Devstral 2), context-prep
  *hurt* — success collapsed to 2/8–3/8 while no-roux held 6/8. A weak agent
  can't exploit the skeleton and is confused by it. The savings story is real for
  capable agents; it is not universal.
- **Cache-read priced at face value** in some aggregations; real cached-input
  pricing makes the prefix approach look better still, not worse.
- Pricing is list price as of mid-2026 and is for *comparing arms*, not a billing
  forecast.

## Reproduce

```bash
# strong-agent context-prep vs baseline (the headline)
ARMS=no-roux,context-prep ONLY_PERSONAS=ripgrep python3 bench/token_economics.py
python3 bench/aggregate_token_economics.py bench/results/<latest>.jsonl

# second-repo run to break the single-repo caveat (roux-l5rf / roux-uqus)
ARMS=no-roux,with-roux,context-prep,context-prep-compact,context-prep-bodies,context-prep-scores \
  ONLY_PERSONAS=ripgrep,pandas ONLY_AGENTS=codex,vibe \
  python3 bench/token_economics.py
```

The `context-prep` arm injects `roux query --format skeleton` verbatim (the same
bytes the `roux://skeleton/{query}` MCP resource serves) and `context-prep-compact`
injects `roux query --format compact` (the bytes `roux_query(compact=true)`
returns), so both measured blocks match production rather than a Python
re-implementation.

Raw runs cited: `bench/results/token_economics_2026062{1T2004,2T1446,2T1803,2T1958}.jsonl`.
