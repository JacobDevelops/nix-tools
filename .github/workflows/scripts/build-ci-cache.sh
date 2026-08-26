#!/usr/bin/env bash
set -euo pipefail

mapfile -t checks < <(nix eval --json '.#checks.x86_64-linux' --apply builtins.attrNames | jq --raw-output '.[]')
installables=()
for check in "${checks[@]}"; do
  installables+=(".#checks.x86_64-linux.$check")
done
mapfile -t example_checks < <(nix eval --json './examples/bun-monorepo#checks.x86_64-linux' --apply builtins.attrNames | jq --raw-output '.[]')
for check in "${example_checks[@]}"; do
  installables+=("./examples/bun-monorepo#checks.x86_64-linux.$check")
done
mapfile -t outputs < <(nix build --no-link --print-out-paths "${installables[@]}")
cache="$RUNNER_TEMP/ci-cache"
mkdir -p "$cache"
nix copy --to "file://$cache?compression=zstd&parallel-compression=true" "${outputs[@]}"
printf '%s\n' "${outputs[@]}" > "$cache/roots"
