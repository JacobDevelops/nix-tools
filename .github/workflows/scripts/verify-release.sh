#!/usr/bin/env bash
set -euo pipefail

version=$(nix eval --raw --impure --expr \
  '(builtins.fromTOML (builtins.readFile ./crates/bun2nix/Cargo.toml)).package.version')
tag="bun2nix-v$version"
printf 'release-sha=%s\n' "$RELEASE_SHA" >> "$GITHUB_OUTPUT"
printf 'tag=%s\n' "$tag" >> "$GITHUB_OUTPUT"

release_status=$(curl --silent --show-error --location \
  --header "Accept: application/vnd.github+json" \
  --header "Authorization: Bearer $GH_TOKEN" \
  --header "X-GitHub-Api-Version: 2022-11-28" \
  --output "$RUNNER_TEMP/release.json" --write-out '%{http_code}' \
  "$GITHUB_API_URL/repos/$GH_REPO/releases/tags/$tag")
case "$release_status" in
  200) exit 0 ;;
  404) ;;
  *)
    printf 'release lookup failed with HTTP %s\n' "$release_status" >&2
    exit 1
    ;;
esac

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
