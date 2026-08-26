#!/usr/bin/env bash
set -euo pipefail

version=$(nix eval --raw --impure --expr \
  '(builtins.fromTOML (builtins.readFile ./crates/bun2nix/Cargo.toml)).package.version')
tag="bun2nix-v$version"
printf 'release-sha=%s\n' "$RELEASE_SHA" >> "$GITHUB_OUTPUT"
printf 'tag=%s\n' "$tag" >> "$GITHUB_OUTPUT"

systems=$(nix eval --json '.#packages' --apply builtins.attrNames)
for attempt in $(seq 1 20); do
  verified=true
  while IFS= read -r system; do
    output=$(nix eval --raw ".#packages.$system.bun2nix.outPath")
    probe="$RUNNER_TEMP/release-cache-$system"
    if ! nix copy --from "$PUBLIC_CACHE" --to "file://$probe" "$output" \
      --option require-sigs true --option trusted-public-keys "$PUBLIC_KEY"; then
      verified=false
      break
    fi
  done < <(jq --raw-output '.[]' <<< "$systems")
  if "$verified"; then
    exit 0
  fi
  test "$attempt" -lt 20
  sleep 30
done
