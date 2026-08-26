#!/usr/bin/env bash
set -euo pipefail

verification_store="local?root=$RUNNER_TEMP/verified-store"
while IFS= read -r output; do
  nix copy --from "$PUBLIC_CACHE" --to "$verification_store" "$output" \
    --option trusted-public-keys "$PUBLIC_KEY"
  nix store verify --store "$verification_store" --recursive \
    --sigs-needed 1 --trusted-public-keys "$PUBLIC_KEY" "$output"
done < "$CACHE_DIRECTORY/roots"
