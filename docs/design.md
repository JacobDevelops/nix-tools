# Design

`nix-tools` is a collection of reusable components, not a command that every repository must adopt.

## Boundaries

The Rust core owns mechanics that are expensive to get right repeatedly: bounded process execution, cancellation, redaction, atomic filesystem changes, portable Nix system names, and build-plan scheduling. It does not know application names, repository layouts, CI providers, cloud accounts, or binary caches.

Repository CLIs depend on `nix-tools-core` and provide those policies themselves. A LOKE CLI can therefore keep a short project-specific name and commands without making either part of the public API.

The Nix framework returns ordinary attrsets. Its package, check, and app outputs can be merged with handwritten Go, Bun, Flutter, infrastructure, or deployment outputs. It is intentionally not a flake module with a second configuration language.

## Bun dependency caches

`bun2nix` separates four concerns:

1. Rust parses the committed Bun lockfile and emits deterministic fetchers and per-workspace dependency closures.
2. Rust creates cache entry names matching Bun's global install cache contract.
3. Nix filters packages for the host platform and rejects a filter decision the lockfile metadata cannot justify.
4. Nix groups dependencies by their exact workspace consumer set, extracts each group once in parallel, and joins only the required groups for each workspace.

This keeps shared dependencies shared without coupling every workspace build to one monolithic cache. Workspace source paths are excluded because Bun resolves them from the filtered project source, and including them would make source edits invalidate dependency caches.

Production derivations do not run tests. Tests, Clippy, formatting, Nix evaluation, and integration fixtures are explicit flake checks so a successful package build never stands in for verification.

## Extension model

Future cache evaluation and publication code belongs behind configuration and narrow traits:

- cache URLs, trust roots, and retention are consumer policy;
- signing and object storage are adapters;
- CI fan-out rendering is a provider adapter over a provider-neutral schedule;
- repository target selection is injected rather than inferred from a fixed naming convention.

That boundary lets repositories share the expensive Nix graph and scheduling work while retaining their existing commands and infrastructure.
