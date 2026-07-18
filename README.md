# <img src="logo.svg" height="28" alt="roux"> roux

**the base your coding agents build on**

[![CI](https://github.com/iicky/roux/actions/workflows/ci.yaml/badge.svg)](https://github.com/iicky/roux/actions/workflows/ci.yaml)
[![codecov](https://codecov.io/gh/iicky/roux/graph/badge.svg)](https://codecov.io/gh/iicky/roux)

`roux` is a single-binary, CPU-only code retrieval tool for AI coding agents. It
builds a graph of your code and its dependencies with [tree-sitter], indexes it
with full-text (BM25) search, and ranks results with Personalized PageRank over
the call/definition graph. Ask it a question in plain language and it returns
the most relevant symbols **plus their graph neighborhood** — callers, callees,
and enclosing types — so an agent gets a map, not just a line hit.

No embeddings, no GPU, no model to download, no network after the initial fetch.
One static binary and a SQLite file per project.

## Why roux

- **A call graph, not a line hit.** Every result carries its 2-hop neighborhood
  — callers, callees, enclosing types — so an agent gets a map of the code, not
  a lone match. There is no `grep` equivalent; this is roux's durable edge.
- **Your dependencies, not just your tree.** `roux init` detects the project
  type and indexes the *source of the libraries you import*, so an agent can
  read the crate it's calling — not just your usage of it. Most tools stop at
  the workspace boundary.
- **Natural-language queries.** Ask `"how does buffered reading work"` instead
  of guessing the identifier. A literal `grep` of a full sentence finds nothing;
  roux ranks the symbols whose names, signatures, and docs match.
- **Built for agents.** Runs as an MCP server, or renders a compact,
  prompt-cacheable skeleton you inject once at the start of a task.
- **No embedding stack.** CPU-only, offline, deterministic — no GPU, no model
  download, no vector database, no reindex lag. One static binary and a SQLite
  file per project.

## How roux compares

roux sits between two tools you already know — line search and embedding RAG —
and takes what each is missing:

| | roux | grep / ripgrep | embedding RAG (Cursor, Cody, vector DBs) |
|---|---|---|---|
| Natural-language queries | Yes | No (literal patterns) | Yes |
| Returns a call graph, not just matches | Yes | No | No (opaque chunks) |
| Indexes your dependencies' source | Yes | Only files on disk | Rarely |
| Offline, CPU-only, no GPU | Yes | Yes | No |
| Deterministic, no reindex drift | Yes | Yes | No |
| Infrastructure to run | One binary + SQLite | None | Vector DB / service / model |

- **vs `grep`/ripgrep** — grep returns a line; roux returns the *symbol, its
  neighbors in the call graph, and the same for the libraries you import*. It is
  line search that understands structure and doesn't stop at your repo boundary.
- **vs embedding RAG** — the natural-language retrieval of semantic search
  without the embedding stack (nothing to host, no GPU, no reindex lag), handing
  the agent a call graph instead of opaque chunks.

roux makes no token-savings claim; see [Benchmarks](#benchmarks) for what it
measurably does (retrieval quality) and [docs/token-economics.md] for the
honest, directional cost analysis.

## Install

### From source

```sh
git clone https://github.com/iicky/roux
cd roux
cargo install --path .        # installs the `roux` binary
# or: cargo build --release   # -> target/release/roux
```

Requires a recent stable Rust toolchain (edition 2024, Rust 1.88+).

### Prebuilt binaries

Prebuilt binaries for macOS and Linux are published on the
[Releases](https://github.com/iicky/roux/releases) page as versions are tagged.

## Quickstart

Under two minutes from an empty checkout:

```sh
cd your-project
roux init                       # index this project's dependencies (project-local)
roux query "where is retry backoff configured"
```

`roux init` writes to a project-local `.roux/db.sqlite` by default. Pass
`--global` to write to a shared store instead, or `--transitive` to include the
full dependency tree.

To index an arbitrary crate or path directly:

```sh
roux add serde                  # a crate by name
roux add ./src                  # a local directory or file
roux query "custom deserializer" --source serde
```

## Commands

| Command | What it does |
|---|---|
| `roux init` | Detect the project type and index its dependencies into `.roux/db.sqlite` (`--global` for the shared store, `--transitive` for the full tree) |
| `roux add <source>` | Index a crate by name, or a local directory/file |
| `roux query <query>` | Retrieve the most relevant symbols for a query |
| `roux list` | List indexed sources |
| `roux sync` | Re-read the lockfile and re-ingest changed dependencies |
| `roux update` | Incrementally refresh path/file sources from the working tree |
| `roux remove <source>` | Remove a source and all its chunks |
| `roux serve` | Run as an MCP server over stdio for agent integration |
| `roux export --output <path>` | Export the local index to a portable artifact (`--gzip` to compress) |
| `roux audit` | Audit public symbols for agent-legibility and recommend refactors (`--check` to gate CI) |

Useful `query` flags:

- `--top N` — number of results (default 5).
- `--also "variant"` — fuse an extra phrasing into the same query via
  reciprocal-rank fusion. Repeatable; the main lever when the code's vocabulary
  differs from the question's.
- `--format text|json|skeleton|compact` — human table (default), machine JSON, a
  one-shot prompt-prefix block, or a budgeted compact block with neighbor names.
- `--source NAME` — restrict to one indexed source.
- `--local` / `--global` — pick the project-local or shared store.
- `--db PATH` — query a specific `.sqlite` index (e.g. a downloaded artifact).

## Supported languages

Rust, Python, JavaScript, TypeScript, Go, C/C++, and Bash — each via its
tree-sitter grammar, so extraction is real parsing, not regex.

## Use with agents

### As an MCP server

Point any MCP client at `roux serve`. A generic stdio config:

```json
{
  "mcpServers": {
    "roux": {
      "command": "/absolute/path/to/roux",
      "args": ["serve", "--local"]
    }
  }
}
```

This exposes one tool, `roux_query` (compact text by default; pass `json: true`
for the full graph with ids, scores, edges, and bodies), and a
`roux://skeleton/{query}` **resource** for one-shot context injection.

### As a one-shot context preprocessor

Rather than call a tool every turn, render a ranked skeleton once at the start of
a task and inject it into the prompt prefix:

```sh
roux query "the feature I'm about to work on" --format skeleton
```

The block is deterministic and prompt-cacheable, so it is written once and read
cheaply on every subsequent turn. See [docs/token-economics.md] for the
(directional, single-repo) cost analysis behind this pattern — treat it as a
supported workflow, not a headline savings claim.

## Audit agent-legibility

Code an agent can't retrieve might as well not exist. `roux audit` turns roux's
own retrieval on your public API: for each symbol it probes how findable you are,
then recommends the cheapest fix for the ones that miss.

```sh
roux audit                    # audit the current tree
roux audit src --format json  # machine-readable findings
```

Every miss is classified by cause, worst-first:

| Cause | What it means | Fix |
|---|---|---|
| **collision** | Buried even for a query built from its own name — same-named siblings out-compete it | Rename or add a distinctive term (docs alone won't help) |
| **vocab gap** | Findable by name, but an intent query phrased *without* that name misses it — no doc bridges concept to code | Add one honest doc line naming what it does |
| **isolation** | No callers/callees/children in the graph — reachable only by exact lexical match | Wire it into the call graph, or document it |

The doc-bridge is the cheap win: on this repo, a single doc line moved
`stem_variants` from #5 to #1 for an intent query that never mentions "stem".
Supply your own intent phrasings with `--queries <file.toml>` — a TOML table
whose (quoted) keys are qualified names, e.g.
`"roux_cli::graph::store::stem_variants" = ["match plural word endings"]` — and
gate CI on findability with `roux audit --write-baseline` / `roux audit --check`.

## Benchmarks

On a held-out query set across five real repositories — ripgrep (Rust), pandas
(Python), Remix (TypeScript), gin (Go), and Marlin (C++) — roux reaches an
aggregate **Hit@10 of 97.5%** (MRR 0.68). On the largest codebase, pandas
(~36k symbols), it reaches **Hit@10 100%** (MRR 0.91). These are retrieval-quality
numbers, reproducible from the checked-in gold sets and index snapshots under
[`bench/`](bench/); they are not a token-cost claim.

## How it works

1. **Extract.** tree-sitter parses each file into symbols (functions, types,
   methods, docs) with their spans, signatures, and doc comments.
2. **Graph.** Symbols become nodes; references, calls, and containment become
   edges — a code graph, persisted in SQLite.
3. **Retrieve.** A query runs BM25 full-text search over names, signatures, and
   docs to seed nodes, expands a bounded ego-graph around them, runs Personalized
   PageRank, and fuses the two scores (`BM25^α × PPR^β`) to rank the result.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

[tree-sitter]: https://tree-sitter.github.io/tree-sitter/
[docs/token-economics.md]: docs/token-economics.md
