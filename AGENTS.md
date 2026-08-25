# nix-tools

Public Rust and Nix tooling shared by JacobDevelops repositories.

## Boundaries

- Keep crates useful without knowledge of a particular company, repository layout, CI provider, cloud, or cache.
- Put policy behind typed configuration and external services behind narrow traits.
- `nix-tools-core` is a library. Repository CLIs compose it; it does not own their command names or application registries.
- `bun2nix` owns Bun lockfile parsing, dependency selection, cache layout, and its Nix API.
- Preserve upstream attribution for code derived from nix-community/bun2nix.

## Toolchain

- Use `jj`, never raw Git.
- Use Rust edition 2024, safe Rust only, and separate `*_test.rs` modules.
- Run `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `nix flake check` before each commit.
- Production Nix packages build only; tests belong in explicit flake checks.
- One logical step per conventional local commit. Never push or mutate GitHub without explicit approval in the current turn.

## Quality

- Write the failing test before implementation.
- Keep public APIs narrow and document their invariants.
- Prefer deterministic ordered collections for serialized or generated output.
- Never weaken a lint, skip a test, or hide an error to make a check pass.

