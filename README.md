# nix-tools

Reusable Rust and Nix foundations for fast, reproducible Nix repositories.

This repository deliberately contains libraries rather than a universal repository CLI. A project can depend on the crates and add its own naming, application registry, CI provider, cloud adapters, and cache policy.

## Crates

- `nix-tools-core` provides safe process execution, atomic filesystem publication, Nix progress primitives, and build scheduling without repository-specific policy.
- `bun2nix` converts Bun lockfiles, computes per-workspace dependency closures, creates Bun cache entries, and exposes optimized Nix builders that avoid one derivation per package.

Both crates are currently consumed from Git while their APIs settle:

```toml
[dependencies]
nix-tools-core = { git = "https://github.com/JacobDevelops/nix-tools" }
```

The flake exports the `bun2nix` CLI, an overlay, and library functions for Bun dependency caches.

## Development

```sh
nix develop
cargo test --workspace
nix flake check
```

