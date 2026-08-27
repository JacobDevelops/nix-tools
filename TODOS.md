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

- Replace thread-per-stream and timed child polling with a measured event-driven implementation
  using safe `nix` APIs where possible and narrowly contained OS bindings only where required.
- Add a zero-copy `exec` path for realized applications when supervision is not requested.
- Stream typed derivation graph decoding and stop cloning graph payloads into progress and manifests.
- Return probe metrics without cloning complete captured process results.
- Deduplicate Bun sources before prefetch, prefetch with bounded concurrency, and compute production,
  check, and development closures from shared indexes/traversals.
- Require before/after CPU, allocation, syscall, wall-time, and cancellation benchmarks for each
  optimization.

## Persistent or native Nix integration experiment

- After the batched CLI implementation has stable p50/p95 baselines, prototype a persistent
  evaluator and, separately, the narrowest supportable native Nix API integration.
- Include startup, repeated invocation, memory, cancellation, Nix-version compatibility, packaging,
  and failure-isolation measurements.
- Adopt either approach only when it materially outperforms the batched CLI path without weakening
  safe-Rust guarantees or creating an unstable public ABI.
