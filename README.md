# nix-tools

Reusable Rust and Nix foundations for fast, reproducible Nix repositories.

This repository provides the shared machinery for optimized Nix repositories without imposing one repository's command tree. The reference CLI has `build`, `check`, and `run`; project CLIs compose the same services with their own selectors, output, and additional subcommands.

## Crates

- `nix-tools-core` provides safe process execution, atomic filesystem publication, history, and provider-neutral scheduling.
- `nix-tools-engine` provides bounded flake discovery, batched evaluation, derivation-graph validation, trusted-cache probes, and dependency-first realization.
- `nix-tools` provides composable `build`, `check`, and `run` services plus the thin reference binary.
- `nix-tools-cache` provides exact NAR serialization and policy-free signed binary-cache publication ports.
- `bun2nix` converts Bun lockfiles, computes workspace closures and consumer sets, creates Bun cache entries in Rust, and exposes an inspection plan.

The crates are currently consumed from Git while their APIs settle:

```toml
[dependencies]
nix-tools-core = { git = "https://github.com/JacobDevelops/nix-tools" }
nix-tools-engine = { git = "https://github.com/JacobDevelops/nix-tools" }
```

## Nix library

The flake exports plain functions that consumers can combine with handwritten outputs:

- Rust dependency cones with shared Crane artifacts and separate package, Clippy, test, and format derivations.
- Collision-checked package/check/app/development-shell target merging.
- Bun host filtering, exact-consumer cache shards, isolated offline installs, and per-workspace build, bundle, test, run, and development outputs.

The [Rust framework example](examples/framework) shows independent Cargo cones. The [Bun monorepo example](examples/bun-monorepo) has shared and workspace-exclusive registry dependencies with production sources isolated from test sources.

The reproducible [monorepo benchmark harness](docs/benchmarks.md) compares `nix-tools` with plain Nix and optional `nix-fast-build` across shared and exclusive dependency graphs.

The flake also exports pinned `bun`, `bun2nix`, and `nix-tools` packages plus apps and an overlay. See the [CLI services](docs/cli.md), [Bun guide](docs/bun.md), and [design boundaries](docs/design.md).

The [Bun guide](docs/bun.md#use-the-prebuilt-cli) shows how to run, install, or pin the prebuilt `bun2nix` CLI. Its closures are served from the [signed public binary cache](docs/binary-cache.md) for x86_64 and ARM64 Linux plus ARM64 macOS.

CI runs the same flake gates and command smoke tests through GitHub Actions on [Blacksmith](https://docs.blacksmith.sh/blacksmith-runners/overview), with Magic Nix Cache using Blacksmith's colocated Actions cache.

## Development

```sh
nix develop
cargo test --workspace
nix flake check
```

Inside the development shell, the reference CLI is available as `nix-tools` or `nt`.
Wrangler is also included; run `wrangler login` once and verify the Cloudflare session with `wrangler whoami` before managing R2.
