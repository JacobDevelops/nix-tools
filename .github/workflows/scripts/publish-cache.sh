#!/usr/bin/env bash
set -euo pipefail

test -n "$AWS_ACCESS_KEY_ID"
test -n "$AWS_SECRET_ACCESS_KEY"
test -n "$NIX_CACHE_PRIVATE_KEY"
umask 077
key_file="$RUNNER_TEMP/nix-tools-cache.sec"
trap 'rm -f "$key_file"' EXIT
printf '%s\n' "$NIX_CACHE_PRIVATE_KEY" > "$key_file"
unset NIX_CACHE_PRIVATE_KEY
derived_public_key=$(nix key convert-secret-to-public < "$key_file")
test "$derived_public_key" = "$PUBLIC_KEY"
destination="s3://$R2_BUCKET?scheme=https&endpoint=$R2_ACCOUNT_ID.r2.cloudflarestorage.com&region=auto&priority=30&compression=zstd&parallel-compression=true&secret-key=$key_file"
while IFS= read -r output; do
  nix copy --from "file://$CACHE_DIRECTORY" --to "$destination" "$output"
done < "$CACHE_DIRECTORY/roots"
