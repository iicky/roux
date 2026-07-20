# Consuming prebuilt roux indexes

An **agent** that wants retrieval context for a codebase has two choices:
index it locally (slow, requires a dev toolchain), or download a prebuilt
index the repo's CI has already produced. This doc covers the second path.

## Artifact format (spec v1)

A roux artifact is a **SQLite file** using the `GraphStore` schema, optionally
gzip-wrapped (`.sqlite.gz`). Beyond the normal tables, the `metadata` table
carries manifest rows identifying the producer:

| Key | Required | Value |
|-----|----------|-------|
| `artifact_version` | yes | Spec version — currently `"1"` |
| `artifact_roux_version` | yes | semver of the producing `roux` binary |
| `artifact_schema_version` | yes | Integer schema version |
| `artifact_created_at` | yes | Unix timestamp (seconds) |
| `artifact_commit` | no | Git SHA of the source repo |
| `artifact_repo` | no | Repo identifier (`owner/name`) |

Compatibility rules consumers enforce (see `src/artifact.rs`):

- Missing `artifact_version` — not a roux artifact.
- `artifact_version != "1"` — refuse; upgrade one side.
- `schema_version` greater than this binary supports — refuse with upgrade hint.
- Older schema — migrated up automatically on open.

## Producing an artifact (CI)

```bash
roux init --local                              # build .roux/db.sqlite
roux export --output roux-index.sqlite.gz --gzip
```

See `.github/workflows/publish-roux-index.yml.example` for a complete workflow
that attaches the artifact to GitHub releases.

## Consuming an artifact (agent)

Download the artifact, decompress if needed, and point `roux` at it:

```bash
curl -L -o idx.sqlite.gz \
  https://github.com/OWNER/REPO/releases/latest/download/roux-index.sqlite.gz
gunzip idx.sqlite.gz

roux query "auth middleware" --db idx.sqlite --format json --top 5
roux list --db idx.sqlite
```

`--db` works on `query` and `list`. The artifact is read-only to the agent —
it is never merged into the agent's local or global store.

## Artifact size & caching

Uncompressed: roughly **1 KB per indexed symbol**. A medium repo with its
direct deps indexed lands in the 1–50 MiB range compressed. Gzip typically
cuts size by 3–4×.

Agents should cache artifacts by URL + `ETag` / `Last-Modified`. The
`artifact_created_at` and `artifact_commit` rows let an agent report which
snapshot it's querying against.
