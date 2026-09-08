# TODOs

## Versioned engine protocol and Go client

- Add a headless `nix-tools` engine command with a versioned, streaming JSON protocol over stdio.
- Define request, progress, result, and error envelopes with request IDs, capability negotiation,
  cancellation, stable error categories, exit status, signals, and explicit repository/Nix policy.
- Reject non-portable arguments explicitly rather than silently coercing `OsString` values.
- Add golden compatibility fixtures and document the additive-field and protocol-version policy.
- Provide a minimal pure-Go client that preserves `CGO_ENABLED=0` and leaves Cobra command ownership
  with the consuming repository.

## Consumer migrations

- Migrate `../jfit` first to validate the language-neutral protocol while retaining its dotenv,
  e2e, proto, and repository-specific commands.
- Consolidate generic Nix execution from `../tools`'s `lt-nix` onto the Rust crates without moving
  AWS, registry, or repository policy into `nix-tools`.
- Adopt the Rust crates from `../atlas` without coupling Atlas to the reference CLI or Clap tree.
- Choose an immutable distribution model for the Rust crates: published releases or pinned tags and
  revisions, backed by an MSRV and semantic-version compatibility policy.

## Process-runner and allocation optimization

- [x] Replace thread-per-stream and timed child polling with a measured event-driven implementation
  using safe `nix` APIs where possible and narrowly contained OS bindings only where required.
- [x] Add a zero-copy `exec` path for realized applications when supervision is not requested.
- [x] Stream typed derivation graph decoding and stop cloning graph payloads into progress and manifests.
- [x] Return probe metrics without cloning complete captured process results.
- [x] Deduplicate Bun sources before prefetch, prefetch with bounded concurrency, and compute production,
  check, and development closures from shared indexes/traversals.
- [x] Require before/after CPU, allocation, syscall, wall-time, and cancellation benchmarks for each
  optimization.

## Realization interface

- [x] Stream build log lines to the interface. `activity.rs` already parses `resBuildLogLine` and
  `resPostBuildLogLine` into a per-derivation `BoundedLog`, but that log is only read to build a
  failure diagnostic, so the selected panel has nothing to show. Add a log-line progress event, a
  bounded per-job ring in the interface model, and a scrollable region in the panel. Bound the
  interface copy by line count rather than inheriting the engine's byte budget: a live tail wants
  the last N lines, not the head-and-tail excerpt a post-mortem wants.
- [x] Distinguish `AwaitingResult` from `Queued` in the job list. Both render `DarkGray`, so a
  derivation that has finished building looks almost identical to one that has not started.
- [x] Report every failing derivation, not the first diagnostic. `manifest_result` takes the first
  error diagnostic and discards the rest, and the batch-level `nix build failed with Exited(1)`
  sorts ahead of the per-derivation ones, so a failed run prints the least informative error it
  holds and none of the collected log excerpts. Print each failed node with its excerpt, and keep
  the batch error as context rather than as the message.
- [x] Settle derivations as they finish rather than when the batch does. `realize_nodes` issues one
  `nix build` for every node, and nix reports per-derivation success only when that call returns,
  so every completed derivation sits at `awaiting result` with a frozen elapsed time until the
  whole batch ends. Derive a provisional outcome from the activity stream's build-stop events and
  correct it from the final JSON.
- [x] Show proven-local jobs as cached before remaining jobs run, preserving state, logs, and
  selection when the derivation graph expands.

Implementation measurements and tradeoffs are recorded in [the benchmark report](docs/benchmarks.md).

## Persistent or native Nix integration experiment

- After the batched CLI implementation has stable p50/p95 baselines, prototype a persistent
  evaluator and, separately, the narrowest supportable native Nix API integration.
- Include startup, repeated invocation, memory, cancellation, Nix-version compatibility, packaging,
  and failure-isolation measurements.
- Adopt either approach only when it materially outperforms the batched CLI path without weakening
  safe-Rust guarantees or creating an unstable public ABI.
