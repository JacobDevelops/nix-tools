# Bun on Nix

`bun2nix` treats Bun as a runtime, bundler, test runner, and package manager. The Rust CLI turns the textual `bun.lock` into fetchers and metadata:

```sh
bun install
bun2nix --output bun.nix
bun2nix inspect --output bun-plan.json
```

## Use the prebuilt CLI

Run the latest `bun2nix` once without installing it:

```sh
nix run --accept-flake-config github:JacobDevelops/nix-tools/v0.1.0#bun2nix -- --output bun.nix
```

Or add it to your user profile:

```sh
nix profile add --accept-flake-config github:JacobDevelops/nix-tools/v0.1.0#bun2nix
bun2nix --version
```

For a repository, pin `nix-tools` in `flake.lock` and expose its package through the development shell:

```nix
{
  nixConfig = {
    extra-substituters = [ "https://nix-tools-cache.jacobdevelops.com" ];
    extra-trusted-public-keys = [
      "nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    nix-tools.url = "github:JacobDevelops/nix-tools/v0.1.0";
  };

  outputs =
    { nixpkgs, nix-tools, ... }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in
    {
      devShells = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        {
          default = pkgs.mkShell {
            packages = [ nix-tools.packages.${system}.bun2nix ];
          };
        }
      );
    };
}
```

Then lock and enter the shell:

```sh
nix flake lock
nix develop --accept-flake-config
bun2nix --output bun.nix
```

Commit `flake.lock`; it pins the exact `nix-tools` revision whose binary is fetched. Input flakes cannot add trusted substituters to their parent, so the consuming root must declare the cache URL and public key shown above. Do not make `nix-tools/nixpkgs` follow the consuming repository's Nixpkgs input: changing that dependency changes the `bun2nix` store path and can force a local source build instead of a cache substitution.

Prebuilt closures are published for x86_64 Linux, ARM64 Linux, and ARM64 macOS. See the [binary-cache documentation](binary-cache.md) for the trust and publishing model.

The flake exports a pinned current Bun package for Linux and macOS, so frozen installs use the same lockfile format regardless of the Bun version currently packaged by nixpkgs.

The generated `bun.nix` contains package sources, lockfile versions 0 through 3, local-package identities, platform restrictions, separate production/check/development closures, and exact consumer sets. Production closures omit dev dependencies, check closures add the selected workspace's dev dependencies, and development closures also add root tooling. It is generated data and is intentionally excluded from repository formatting.

The Nix library uses that metadata to:

- remove local workspace sources from dependency caches;
- remove packages whose `os` or `cpu` restrictions exclude the host;
- group dependencies by exact workspace consumers;
- extract every group once in parallel rather than create one derivation per package;
- preserve private-registry cache names;
- join only the shards one workspace needs;
- perform isolated, frozen, offline installs;
- expose caller-configured build, bundle, test, run, and development outputs.

Lifecycle scripts are ignored by default. Consumers opt in explicitly and can supply a separate lifecycle phase. A no-op `bun2nix` shadows root regeneration scripts during sandboxed installs, while the real Rust binary creates cache entries before installation.

Production and check sources and dependency caches are separate inputs. Production installs pass `--production`; test-only source or dev-dependency edits therefore invalidate checks without adding dev-only packages to production caches. Patches are rejected until patched cache entries can be reproduced exactly. Global `link:` dependencies are rejected because Bun omits their registered source path from `bun.lock`; private-registry hosts longer than 32 bytes are rejected because the lockfile omits the registry URL needed for Bun's cache hash. Source URLs containing credentials are also rejected. See the [Bun monorepo example](../examples/bun-monorepo) for shared, dev-only, and workspace-exclusive dependencies across two apps.
