#!/usr/bin/env bash
set -euo pipefail

actual_system=$(nix eval --raw --impure --expr builtins.currentSystem)
test "$actual_system" = "$EXPECTED_SYSTEM"
output=$(nix build --no-link --print-out-paths .#bun2nix)
test "$output" = "$EXPECTED_OUTPUT"
cache="$RUNNER_TEMP/bun2nix-cache"
mkdir -p "$cache"
nix copy --to "file://$cache?compression=zstd&parallel-compression=true" "$output"
printf '%s\n' "$output" > "$cache/roots"
