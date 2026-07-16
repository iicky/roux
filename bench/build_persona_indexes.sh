#!/usr/bin/env bash
# Build on-disk roux indexes for ALL FIVE persona repos so bench/rank_eval.py
# scores the honest 5-persona set, not the easy 3.
#
# Why this exists: rank_eval.py queries the on-disk index at
# /tmp/roux-sources/<repo>/.roux/db.sqlite. `roux init` cannot create those for
# a JS monorepo (remix) or a manifest-less C++ root (Marlin), so without this script those two silently drop and the headline
# MRR re-inflates from the real 0.791 to the flattering 3-persona 0.859.
#
# `roux add <path>` indexes the project's OWN source (what the personas test);
# `roux init` ingests dependencies and is the wrong tool here. Refs match
# .github/workflows/persona-bench.yml so local == CI.
set -euo pipefail

ROUX="$(dirname "$0")/../target/release/roux"
SRC=/tmp/roux-sources
mkdir -p "$SRC"

# name  ref  lang  url
PERSONAS=(
  "ripgrep 14.1.1       rust       https://github.com/BurntSushi/ripgrep"
  "pandas  v2.2.3       python     https://github.com/pandas-dev/pandas"
  "remix   remix@2.13.1 typescript https://github.com/remix-run/remix"
  "gin     v1.10.0      go         https://github.com/gin-gonic/gin"
  "Marlin  2.1.2.5      cpp        https://github.com/MarlinFirmware/Marlin"
)

for row in "${PERSONAS[@]}"; do
  read -r name ref lang url <<<"$row"
  dir="$SRC/$name"
  if [ ! -d "$dir" ]; then
    echo "[clone] $name @ $ref"
    git clone --depth=1 --branch="$ref" "$url" "$dir"
  fi
  echo "[index] $name ($lang)"
  ( cd "$dir" && "$ROUX" add . --lang "$lang" --local --name "$name" )
done

echo "[done] all 5 persona indexes built. Run: python3 bench/rank_eval.py"
