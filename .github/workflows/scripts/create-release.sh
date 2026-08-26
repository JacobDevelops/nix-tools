#!/usr/bin/env bash
set -euo pipefail

install="nix run --accept-flake-config github:JacobDevelops/nix-tools/$TAG#bun2nix -- --version"
notes=$(printf 'Install from the signed binary cache:\n\n    %s\n\nBinaries are served from [%s](%s).\n' \
  "$install" "$CACHE_URL" "$CACHE_URL")
if gh release view "$TAG" >/dev/null 2>&1; then
  printf '%s is already released\n' "$TAG"
  exit 0
fi
tag_status=$(curl --silent --show-error --location \
  --header "Accept: application/vnd.github+json" \
  --header "Authorization: Bearer $GH_TOKEN" \
  --header "X-GitHub-Api-Version: 2022-11-28" \
  --output "$RUNNER_TEMP/tag-ref.json" --write-out '%{http_code}' \
  "$GITHUB_API_URL/repos/$GH_REPO/git/ref/tags/$TAG")
case "$tag_status" in
  200)
    tagged_sha=$(gh api "repos/$GH_REPO/commits/$TAG" --jq .sha)
    test "$tagged_sha" = "$RELEASE_SHA"
    gh release create "$TAG" --verify-tag --generate-notes --notes "$notes" --title "$TAG"
    ;;
  404)
    gh release create "$TAG" --target "$RELEASE_SHA" --generate-notes --notes "$notes" --title "$TAG"
    ;;
  *)
    printf 'tag lookup failed with HTTP %s\n' "$tag_status" >&2
    exit 1
    ;;
esac
