# Monorepo benchmarks

`benchmarks/benchmark.py` generates pinned, synthetic flakes and measures the same target graph
through `nix-tools` and plain Nix. It covers 1, 8, 32, and 128 targets in two dependency shapes:
one dependency shared by every target, and one exclusive dependency per target. When
`nix-fast-build` is on `PATH`, the harness also runs the equivalent `checks.<system>` set through
it. Generated fixtures live in a temporary directory and raw JSON belongs under the ignored
`benchmarks/results/` directory.

The engine is given a tracing Nix wrapper. This records its evaluation, recursive derivation-graph,
cache-probe, and realization subprocess totals without changing the engine API. Every outer command
records monotonic wall time, distinct sampled process-tree PIDs, captured output bytes, and peak
summed process-tree RSS on Linux. Recursive `nix derivation show` supplies derivation counts; exact
graph output paths queried with `nix path-info` supply cache reuse; changing one dependency and
comparing target derivation paths supplies invalidation fan-out. Each implementation gets a distinct
recorded run ID so realization is not accidentally a no-op, then is run again unchanged to measure a
no-op rebuild. Results are checkpointed after every scenario, so an interrupted large run remains
explicitly incomplete but retains finished samples.

Run the benchmark from a clean development shell:

```sh
cargo build --release --package nix-tools
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/benchmark.py \
  --run-id "$(date -u +%Y%m%dT%H%M%SZ)" \
  --output benchmarks/results/latest.json
```

Pass `--nix-fast-build /path/to/nix-fast-build` to force that optional comparator. The harness uses
the upstream CLI's complete `#checks.<system>` flake path and non-interactive renderer. One invocation
produces one sample per scenario and implementation; repeat with new run IDs when distributions,
rather than a baseline, are needed.

## Current-worktree baseline

The following single-sample run used the release `nix-tools` binary built from the working tree on
2026-08-26, Nix 2.34.8, x86_64 Linux 7.0, and nixpkgs revision
`56c02bc00adcf003215cc4bd996d6efaf4cff188`. RSS is MiB. Raw output is
`benchmarks/results/measured-2026-08-26.json`, with the separately checkpointed 128-target sample in
`benchmarks/results/measured-128-2026-08-26.json`; both are intentionally ignored.

| Targets | Dependencies | Derivations | Invalidation | `nix-tools` realize / no-op (s) | `nix-tools` processes / RSS | Plain Nix realize / no-op (s) | Plain Nix processes / RSS |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | shared | 574 | 1 | 1.015 / 0.259 | 13 / 120.3 | 0.406 / 0.177 | 1 / 105.6 |
| 1 | exclusive | 574 | 1 | 1.480 / 0.265 | 13 / 119.7 | 0.265 / 0.161 | 1 / 105.8 |
| 8 | shared | 581 | 8 | 2.388 / 0.385 | 25 / 205.3 | 2.209 / 1.040 | 1 / 435.0 |
| 8 | exclusive | 588 | 1 | 2.783 / 0.260 | 25 / 196.2 | 2.303 / 1.476 | 1 / 435.2 |
| 32 | shared | 605 | 32 | 7.259 / 0.339 | 59 / 202.6 | 4.654 / 4.013 | 1 / 1575.2 |
| 32 | exclusive | 636 | 1 | 7.354 / 0.261 | 58 / 206.9 | 6.342 / 4.032 | 1 / 1576.6 |
| 128 | shared | 701 | 128 | 39.268 / 0.308 | 272 / 462.0 | 20.975 / 17.868 | 1 / 5172.6 |
| 128 | exclusive | 828 | 1 | 38.935 / 0.541 | 273 / 471.8 | 23.178 / 16.999 | 1 / 5183.7 |

For the completed samples, graph cache validity moved from 256 to 258 paths at one target, from
258 to 265 (shared) or 272 (exclusive) at eight targets, and from 265 to 289 (shared) or 272 to 320
(exclusive) at 32 targets, and from 479 to 608 (shared) or 735 (exclusive) at 128 targets. Captured
output peaked at 22.4 KiB for `nix-tools` and 151.0 KiB for plain Nix. The engine's warm no-op stayed
between 0.259 and 0.541 seconds while plain Nix rose to about 17 seconds at 128 targets; the
corresponding cold realization remained slower at every measured size. This supports the warm-root
fast path, but not further production optimization from this single sample.

`nix-fast-build` was not installed, so it has no fabricated comparison row.

## Comparable implementation deltas

The ignored `dependency-scheduler-baseline-2026-08-26.json` used the same worktree and harness
structure, but disabled the local-root fast path and restored the old per-dependency scheduler. Its
one-target cold realization results isolate the cost of traversing and individually submitting the
recursive graph:

| Dependencies | Old scheduler wall / Nix builds / OS processes | Current wall / Nix builds / OS processes | Wall reduction |
| --- | ---: | ---: | ---: |
| shared | 92.030 s / 574 / 1,098 | 1.015 s / 1 / 13 | 98.9% (90.7x) |
| exclusive | 15.791 s / 574 / 1,068 | 1.480 s / 1 / 13 | 90.6% (10.7x) |

The ignored `no-fast-path-2026-08-26.json` changed only the warm local-root fast path. Comparing its
no-op wall time with the current sample shows the following deltas:

| Targets | Dependencies | Without fast path | Current | Change |
| ---: | --- | ---: | ---: | ---: |
| 1 | shared | 0.353 s | 0.259 s | 26.5% faster |
| 1 | exclusive | 0.376 s | 0.265 s | 29.5% faster |
| 8 | shared | 0.353 s | 0.385 s | 9.2% slower |
| 8 | exclusive | 0.594 s | 0.260 s | 56.2% faster |
| 32 | shared | 0.353 s | 0.339 s | 4.0% faster |
| 32 | exclusive | 0.595 s | 0.261 s | 56.2% faster |

These are single samples with identical generated graph structure rather than distributions. Small
sub-100 ms differences, especially the eight-target shared regression, are within likely scheduler,
daemon, and sampling noise; the order-of-magnitude scheduler reductions and repeated exclusive
no-op reductions are the stronger signals.

Two observations predate the corrected current-worktree run and are not comparable table samples:
an old failing path returned in 662.7 ms, while an old warm full-check took 23.99 seconds because it
incorrectly traversed work behind already-valid roots. They are retained only as the audit evidence
that motivated the warm-root correctness fix.
