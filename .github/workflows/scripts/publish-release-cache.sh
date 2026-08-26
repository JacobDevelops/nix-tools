#!/usr/bin/env bash
set -euo pipefail

mkdir -p "$CACHE_DIRECTORY"
: > "$CACHE_DIRECTORY/roots"
for cache in "$RUNNER_TEMP"/bun2nix-caches/*; do
  while IFS= read -r output; do
    nix copy --from "file://$cache" --to "file://$CACHE_DIRECTORY" "$output"
    printf '%s\n' "$output" >> "$CACHE_DIRECTORY/roots"
  done < "$cache/roots"
done
.github/workflows/scripts/publish-cache.sh
