# TODOs

## Composable Go SDK and versioned engine protocol

- [x] Provide a typed pure-Go SDK over a headless Rust engine, preserving `CGO_ENABLED=0` and the command composition available to Rust consumers. See [sdk/go](sdk/go/README.md).
- [x] Keep Cobra commands, target selection, repository configuration and preflight checks, trust policy, resource budgets, custom workflows, and progress presentation with the Go consumer. Rust owns discovery, evaluation, dependency graphs, cache probes, realization, and structured results.
- [x] Expose discovery, build, check, and `PrepareRun` through a versioned, streaming JSON protocol over stdio. Return the realized executable and manifest from `PrepareRun`; Go owns app arguments, dotenv loading, working directory, environment, terminal streams, exit status, and signal handling.
- [x] Define request, progress, result, and error envelopes with request IDs, capability negotiation, cancellation, stable error categories, and explicit Nix configuration. Preserve failed and cancelled realization manifests for caller inspection.
- [x] Reject non-portable protocol strings explicitly. Keep native app argument bytes in Go rather than coercing them through JSON.
- [x] Add shared golden compatibility fixtures and document additive fields, unknown events, protocol versions, and SDK/engine compatibility. Test cancellation, partial failures, app execution, and consumer-owned selection through the Go API. See [protocol v1](docs/protocol.md).
- [x] Package a compatible, immutable SDK/engine pair through Nix. The Go client needs no cgo, but requires the Rust engine executable at runtime.
- [x] Expose the shared Rust TUI and stream renderer through the Go SDK. Keep terminal input separate from protocol control, restore the terminal before app handoff, and default jfit's Nix commands to TUI with automatic nonterminal fallback.
- [ ] Specify cache publication separately before claiming full Rust API parity. `../tools` injects store-path lookup, signing, storage, and compression adapters; an engine-only protocol does not expose those seams. Keep publication with the consumer until an explicit cache boundary is designed, without generic Rust callback RPC or provider policy in the engine.

## Consumer migrations

- [x] Migrate `../jfit` discovery, build, check, and run first, retaining its target syntax, dotenv precedence, e2e, proto, and repository-specific commands in Go. The checkout uses a local Go workspace and local Nix inputs for testing.
- [x] Establish behavior parity before switching jfit commands: support `--no-cache` rebuilds, preserve bare `check`'s full flake validation rather than substituting selected check realization, and verify output links, progress, exit status, and cancellation.
- [ ] Replace jfit's local testing inputs with an immutable nix-tools revision and matching Go module version before publishing the migration.
- [x] Migrate jfit CI package/check/Android builds and cache warming's shared CI builds onto the SDK. Preserve explicit installables, build concurrency, all-output selection, remote-cache skipping without forced rebuilds, atomic partial-success result files, and materialized release outputs. Remove nix-fast-build; keep credentials, signing, and publication policy in jfit.
- Consolidate generic Nix execution from `../tools`'s `lt-nix` onto the Rust crates without moving
  AWS, registry, or repository policy into `nix-tools`.
- Adopt the Rust crates from `../atlas` without coupling Atlas to the reference CLI or Clap tree.
- Choose an immutable distribution model for the Rust crates: published releases or pinned tags and
  revisions, backed by an MSRV and semantic-version compatibility policy.

## Persistent or native Nix integration experiment

- After the batched CLI implementation has stable p50/p95 baselines, prototype a persistent
  evaluator and, separately, the narrowest supportable native Nix API integration.
- Include startup, repeated invocation, memory, cancellation, Nix-version compatibility, packaging,
  and failure-isolation measurements.
- Adopt either approach only when it materially outperforms the batched CLI path without weakening
  safe-Rust guarantees or creating an unstable public ABI.
