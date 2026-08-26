# Design

`nix-tools` is a collection of reusable components, not a command tree that every repository must adopt.

## Boundaries

The Rust core owns mechanics that are expensive to get right repeatedly: bounded process execution, cancellation, redaction, atomic filesystem changes, portable Nix system names, and build-plan scheduling. It does not know application names, repository layouts, CI providers, cloud accounts, or binary caches.

The public command services expose three standard operations: `build`, `check`, and `run`. All three use the same evaluator and realizer, so target discovery, derivation deduplication, cache probes, dependency ordering, cancellation, progress, and diagnostics do not drift between commands. The included binary is a reference client. Repository CLIs compose the services beneath it and keep complete ownership of their command names, arguments, selectors, output, and additional subcommands.

The evaluator and realizer are separate from command parsing. They discover standard flake outputs, evaluate targets in bounded batches, validate and deduplicate the derivation graph, probe only configured trusted caches, and submit each missing selected root once so Nix owns dependency-first realization and substitution. Already-local roots skip recursive graph loading. `run` first realizes the derivation carried by an app program's Nix string context, then executes it without bypassing the shared build path.

The Nix framework returns ordinary attrsets. Its package, check, and app outputs can be merged with handwritten Go, Bun, Flutter, infrastructure, or deployment outputs. It is intentionally not a flake module with a second configuration language.

Binary-cache publication is another adapter boundary. Canonical NAR serialization, hashing, signed metadata, integrity checks, dependency-wave scheduling, and selective batch publication are reusable. Caller codecs own compression and encoded object names; signing keys, object stores, URLs, trust roots, retention, and credentials remain consumer policy. Existing narinfo/archive pairs are read and validated before reuse, corrupt pairs are repaired with metadata committed last, and a CI unit publishes only the paths it owns.

## Bun dependency caches

`bun2nix` separates four concerns:

1. Rust parses the committed Bun lockfile and emits deterministic fetchers, package metadata, platform restrictions, and separate production, check, and development closures per workspace.
2. Rust creates cache entry names matching Bun's global install cache contract.
3. Nix filters packages for the host platform and rejects a filter decision the lockfile metadata cannot justify.
4. Nix groups dependencies by their exact workspace consumer set, extracts each group once in parallel, and joins only the required groups for each workspace.
5. Nix uses each workspace cache for offline installs and exposes caller-configured Bun build, bundle, test, and run targets.

This keeps shared dependencies shared without coupling every workspace build to one monolithic cache. Workspace source paths are excluded because Bun resolves them from the filtered project source, and including them would make source edits invalidate dependency caches.

The Bun layer treats Bun as the runtime, bundler, test runner, and package manager. It does not assume Vite, Next.js, a server framework, or an output layout. Consumers provide source cones, workspace identity, scripts, and install outputs; the framework supplies isolated installs, cache reuse, target wiring, and reproducible execution.

Production derivations do not run tests. Tests, Clippy, formatting, Nix evaluation, and integration fixtures are explicit flake checks so a successful package build never stands in for verification.

## Extension model

Provider and repository behavior stays behind configuration and narrow traits:

- cache URLs, trust roots, and retention are consumer policy;
- signing and object storage are adapters;
- CI fan-out rendering is a provider adapter over a provider-neutral schedule;
- repository target selection is injected rather than inferred from a fixed naming convention.

That boundary lets repositories share the expensive Nix graph and scheduling work while retaining their existing commands and infrastructure.

## Performance invariants

- Production packages, tests, lint, and formatting are separate derivations, but reuse dependency artifacts where the language toolchain supports it.
- Source cones include only manifests, lockfiles, compile-time inputs, and the source needed by one target.
- Dependency caches are keyed by lock metadata, host platform, and exact consumers rather than by unrelated application source.
- Evaluation has explicit root, output, memory, batch, and worker limits.
- One derivation is evaluated, realized, and published at most once per plan.
- Cache misses fall back only according to caller policy; untrusted substituters never become an implicit network path.
- A production package build never stands in for its tests.
