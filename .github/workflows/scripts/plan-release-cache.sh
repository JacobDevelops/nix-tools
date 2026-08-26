#!/usr/bin/env bash
set -euo pipefail

version=$(nix eval --raw --impure --expr \
  '(builtins.fromTOML (builtins.readFile ./crates/bun2nix/Cargo.toml)).package.version')
tag="bun2nix-v$version"
release_status=$(curl --silent --show-error --location \
  --header "Accept: application/vnd.github+json" \
  --header "Authorization: Bearer $GH_TOKEN" \
  --header "X-GitHub-Api-Version: 2022-11-28" \
  --output "$RUNNER_TEMP/release.json" --write-out '%{http_code}' \
  "$GITHUB_API_URL/repos/$GH_REPO/releases/tags/$tag")
case "$release_status" in
  200)
    printf '%s is already released\n' "$tag"
    printf 'matrix=%s\n' '{"include":[{"runner":"ubuntu-24.04","system":"none","output":"","skip":true}]}' >> "$GITHUB_OUTPUT"
    printf 'publish-needed=false\n' >> "$GITHUB_OUTPUT"
    exit 0
    ;;
  404) ;;
  *)
    printf 'release lookup failed with HTTP %s\n' "$release_status" >&2
    exit 1
    ;;
esac

missing='[]'
add_missing() {
  missing=$(jq --compact-output \
    --arg runner "$runner" --arg system "$system" --arg output "$output" \
    '. + [{runner: $runner, system: $system, output: $output}]' <<< "$missing")
}

while IFS= read -r target; do
  runner=$(jq --raw-output .runner <<< "$target")
  system=$(jq --raw-output .system <<< "$target")
  output=$(nix eval --raw ".#packages.$system.bun2nix.outPath")
  store_name=${output#/nix/store/}
  narinfo_hash=${store_name%%-*}
  if status=$(curl --fail-with-body --silent --show-error \
    --retry 3 --retry-all-errors --retry-delay 1 --retry-max-time 30 \
    --output "$RUNNER_TEMP/$narinfo_hash.narinfo" --write-out '%{http_code}' \
    "$PUBLIC_CACHE/$narinfo_hash.narinfo"); then
    test "$status" = 200
  else
    if test "$status" = 404; then
      add_missing
      continue
    fi
    printf 'cache probe failed for %s with HTTP %s\n' "$system" "$status" >&2
    exit 1
  fi
  probe="$RUNNER_TEMP/cache-probe-$system"
  nix copy --from "$PUBLIC_CACHE" --to "file://$probe" "$output" \
    --option require-sigs true --option trusted-public-keys "$PUBLIC_KEY"
  printf '%s is already published as %s\n' "$system" "$output"
done < <(jq --compact-output '.[]' <<< "$TARGETS")

if jq --exit-status 'length == 0' <<< "$missing" >/dev/null; then
  matrix='{"include":[{"runner":"ubuntu-24.04","system":"none","output":"","skip":true}]}'
  publish_needed=false
else
  matrix=$(jq --compact-output --null-input --argjson include "$missing" '{include: $include}')
  publish_needed=true
fi
printf 'matrix=%s\n' "$matrix" >> "$GITHUB_OUTPUT"
printf 'publish-needed=%s\n' "$publish_needed" >> "$GITHUB_OUTPUT"
