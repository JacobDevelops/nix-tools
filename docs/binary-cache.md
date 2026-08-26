# Public binary cache

Released `bun2nix` outputs are published to the durable `nix-tools-releases` Cloudflare R2 bucket at `https://releases.nix-tools.jacobdevelops.com`. Successful `main` checks populate the disposable `nix-tools-cache` bucket at `https://cache.nix-tools.jacobdevelops.com` for CI and local development. Both Cloudflare-managed `r2.dev` endpoints remain disabled.

The cache identity is:

```nix
{
  extra-substituters = [
    "https://releases.nix-tools.jacobdevelops.com"
    "https://cache.nix-tools.jacobdevelops.com"
  ];
  extra-trusted-public-keys = [
    "nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM="
  ];
}
```

The root flake declares this configuration and exports the same values as `lib.binaryCache`. Flakes that consume `nix-tools` as an input must repeat the URL and key in their own root `nixConfig`; input flake configuration is not inherited.

## Publishing

`.github/workflows/publish-cache.yml` publishes the exact Nix store path and complete signed runtime closure for each native `bun2nix` package only while its version has no GitHub release. Once released, later commits with the same package version cannot add objects to the release bucket. Build jobs create unsigned local binary caches without credentials; the serialized publisher receives the signing key and R2 credentials.

`.github/workflows/publish-ci-cache.yml` rebuilds the successful `main` revision's x86_64 Linux checks and publishes their closures to the CI bucket. Pull requests can read both caches but cannot access either publishing path. Both publishers use the `cache-publishing` GitHub environment, which must remain restricted to the `main` branch, and require these environment secrets:

- `NIX_CACHE_PRIVATE_KEY`: dedicated `nix-tools-cache-1` signing key.
- `R2_ACCESS_KEY_ID`: R2 S3 access key with write access to both buckets.
- `R2_SECRET_ACCESS_KEY`: matching R2 S3 secret.

The release bucket has no expiration policy. The CI bucket expires every object after 30 days. Separate buckets are required because Nix stores narinfo at the bucket root and archives under `nar/`, so a prefix policy cannot distinguish release objects from CI objects safely.

After every upload, the publisher copies each output and its closure back from the public custom domain into a fresh local store and requires a valid signature from the pinned key. The workflow fails if R2, the custom domain, or the signatures cannot serve a trusted closure.

The R2 writer cannot make clients trust arbitrary bytes by itself. Nix accepts a cache object only when its narinfo signature matches the public key pinned above. Compromise of the signing key is therefore the critical incident: remove the public key from consumers, rotate to a new key name, replace the GitHub secret, and republish known-good outputs.

## Releases

Package-scoped version tags select a source revision; R2 supplies that revision's exact native store paths. After successful `main` CI, the release workflow derives `bun2nix-v<version>` from the independent bun2nix package version and waits until the x86_64 Linux, ARM64 Linux, and ARM64 macOS closures can all be copied from R2 with the pinned signing key. A separate minimal write-token job then creates the tag and release automatically if that version does not already exist. It creates no separate binary artifacts.

GitHub release immutability locks each published tag and its generated release attestation. Consumers should use a package-scoped release created after the cache migration and commit their `flake.lock`; the tag is the human version while the lock records the exact revision and source hash. Earlier releases remain immutable but their prebuilt binaries are unsupported.

## Verification

After publishing, verify the public endpoint and signature from a clean store:

```sh
curl --fail https://releases.nix-tools.jacobdevelops.com/nix-cache-info

nix build \
  --substituters 'https://releases.nix-tools.jacobdevelops.com https://cache.nix-tools.jacobdevelops.com https://cache.nixos.org' \
  --trusted-public-keys 'nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM=' \
  github:JacobDevelops/nix-tools#bun2nix
```
