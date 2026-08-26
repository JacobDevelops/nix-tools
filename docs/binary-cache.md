# Public binary cache

`bun2nix` outputs are published to a signed Nix binary cache in the `nix-tools-cache` Cloudflare R2 bucket. The production endpoint is `https://nix-tools-cache.jacobdevelops.com`; the Cloudflare-managed `r2.dev` endpoint remains disabled.

The cache identity is:

```nix
{
  extra-substituters = [ "https://nix-tools-cache.jacobdevelops.com" ];
  extra-trusted-public-keys = [
    "nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM="
  ];
}
```

The root flake declares this configuration and exports the same values as `lib.binaryCache`. Flakes that consume `nix-tools` as an input must repeat the URL and key in their own root `nixConfig`; input flake configuration is not inherited.

## Publishing

`.github/workflows/publish-cache.yml` evaluates each native `.#bun2nix` Nix store path on a Linux planner and verifies it against the signed public cache. Only an explicit missing narinfo schedules a native builder; transport, service, and signature failures stop before expensive runners start. Build jobs create unsigned local binary caches without receiving credentials for x86_64 and ARM64 Linux plus ARM64 macOS. A single publisher job downloads those caches, signs the complete runtime closures with the dedicated Nix cache key, and copies them to R2 through its S3-compatible endpoint.

Publishing runs only for `main` pushes or explicit manual dispatches of `main`. Pull requests cannot access the publishing path. The publisher uses the `cache-publishing` GitHub environment, which must remain restricted to the `main` branch, and requires these environment secrets:

- `NIX_CACHE_PRIVATE_KEY`: dedicated `nix-tools-cache-1` signing key.
- `R2_ACCESS_KEY_ID`: bucket-scoped R2 S3 access key.
- `R2_SECRET_ACCESS_KEY`: bucket-scoped R2 S3 secret.

Create the R2 token with Object Read & Write permission restricted to the `nix-tools-cache` bucket. Never use the Wrangler OAuth token or an account-wide R2 token in CI.

After every upload, the publisher copies each output and its closure back from the public custom domain into a fresh local store and requires a valid signature from the pinned key. The workflow fails if R2, the custom domain, or the signatures cannot serve a trusted closure.

The R2 writer cannot make clients trust arbitrary bytes by itself. Nix accepts a cache object only when its narinfo signature matches the public key pinned above. Compromise of the signing key is therefore the critical incident: remove the public key from consumers, rotate to a new key name, replace the GitHub secret, and republish known-good outputs.

## Releases

Package-scoped version tags select a source revision; R2 supplies that revision's exact native store paths. After successful `main` CI, the release workflow derives `bun2nix-v<version>` from the independent bun2nix package version and waits until the x86_64 Linux, ARM64 Linux, and ARM64 macOS closures can all be copied from R2 with the pinned signing key. A separate minimal write-token job then creates the tag and release automatically if that version does not already exist. It creates no separate binary artifacts.

GitHub release immutability locks each published tag and its generated release attestation. Consumers should use a version tag such as `bun2nix-v0.2.1` and commit their `flake.lock`; the tag is the human version while the lock records the exact revision and source hash. The legacy unscoped `v0.1.0` release remains immutable, but new consumers should use the package-scoped tag.

## Verification

After publishing, verify the public endpoint and signature from a clean store:

```sh
curl --fail https://nix-tools-cache.jacobdevelops.com/nix-cache-info

nix build \
  --substituters 'https://nix-tools-cache.jacobdevelops.com https://cache.nixos.org' \
  --trusted-public-keys 'nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM=' \
  github:JacobDevelops/nix-tools#bun2nix
```
