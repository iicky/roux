# roux CLI style guide

Internal reference for how roux commands present output. Follow this when adding
or changing a command so the CLI stays consistent.

## The golden rule: stdout is data, stderr is status

- **stdout** carries the *answer* — query results, JSON, skeleton/compact
  blocks, the `list` table, export paths. It must stay machine-readable:
  **no color, no prefixes, no branding** on stdout. A consumer piping
  `roux query … --format json` gets exactly the JSON.
- **stderr** carries *status* — progress, completion, warnings, hints. This is
  where color and the brand mark live. `--quiet` suppresses routine status on
  stderr; it never touches stdout data.

Status output should go through `src/output.rs` (never `eprintln!` a status line
directly); the existing handler `eprintln!` sites are being migrated to it. Data
output stays as plain `println!` in the command handlers.

## Status prefixes

Every status line starts with a one-word, lowercase, colon-free prefix, bold,
one space before the message.

| Prefix  | Color       | Use                                              | `--quiet` |
|---------|-------------|--------------------------------------------------|-----------|
| `❖`     | roux amber  | a completed action — the brand mark              | hidden    |
| `ok`    | green bold  | a check passed / sub-step succeeded              | hidden    |
| `warn`  | yellow bold | non-fatal issue the user should see              | shown     |
| `error` | red bold    | a failure (handler still returns `Err` to exit)  | shown     |
| `hint`  | cyan bold   | next-action suggestion under a `warn`/`error`    | shown     |

Routine progress lines (no prefix, plain) are emitted with `output::step` and
are hidden under `--quiet`. Verbose-only detail uses `output::detail` and shows
only under `--verbose`.

Examples:

```
Indexing local source as 'roux'...
ok indexed serde  (412 symbols, 1130 edges)
❖ init complete — 18 sources, 41k symbols
warn index is stale — 3 sources changed on disk
hint run `roux sync` to refresh
error no index found at .roux/db.sqlite
hint run `roux init` to build one
```

Never write `ok:`, `Error:`, `OK`, or a trailing period on a status line.

## The brand mark

`❖` (roux amber `#C87D3A`) is the completed-action mark — the same glyph as
`logo.svg`. It lives in exactly one place in code, `output::MARK`; changing that
constant rebrands the CLI. Use `❖` for "the whole operation finished"
(`init`/`add`/`sync`/`update` completion), not for every sub-step — sub-steps
use `ok`.

## Formatting

- **Symbol / qualified names** — `bold()`.
- **file:line locations** — plain (they're data even inside a status line).
- **Counts** — plain; right-align when printing a column of counts.
- **Follow-up hints** — the `hint` prefix, or two-space-indented dimmed under
  the parent line.
- **External error text** — dimmed, appended at the end of the line.

## Layout

- No trailing period on status lines.
- No decorative horizontal rules or ASCII art on stderr.
- Blank lines only between logical sections, never between consecutive status
  lines of the same kind.
- Totals/summaries on their own line, after the item list.

## Colors

Always go through `output::*`, which uses the `colored` crate's semantic helpers
(`.green().bold()`, `.dimmed()`) — never raw ANSI escapes. `output::init`
enables styling only when stderr is a TTY and `NO_COLOR` is unset, so piped or
redirected output degrades to plain text.

## Verbosity & exit codes

- `--quiet` — suppress routine status (`❖`/`ok`/`step`); keep `warn`/`error`/
  `hint`. Data on stdout is unaffected.
- `--verbose` — enable `detail` lines and `tracing` debug logs (stderr).
- Exit `0` on success; handlers return `Err` (→ non-zero) on failure. The
  `error` prefix is printed alongside a returned `Err`, not instead of it.
