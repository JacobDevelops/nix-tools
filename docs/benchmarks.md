# Monorepo benchmarks

`benchmarks/benchmark.py` generates pinned, synthetic flakes and measures the same target graph
through `nix-tools` and plain Nix. It covers 1, 8, 32, and 128 targets in two dependency shapes:
one dependency shared by every target, and one exclusive dependency per target. When
`nix-fast-build` is on `PATH`, the harness also runs the equivalent `checks.<system>` set through
it. Generated fixtures live in a temporary directory and raw JSON belongs under the ignored
`benchmarks/results/` directory.

The harness defaults to five independent samples per scenario (`--repeats N`). Each sample gets a
separately salted fixture, so its first realization remains cold instead of later samples silently
becoming no-op rebuilds. Raw samples are retained and every numeric leaf is summarized with min,
p50, p95, and max. The harness checkpoints only after a complete repeated scenario.

The harness runs a cold and warm engine pair through a tracing Nix wrapper to attribute its
evaluation, recursive derivation-graph, cache-probe, and realization subprocess totals without
changing the engine API. It uses a separately salted fixture for that trace, then measures the engine
against real Nix so wrapper overhead does not bias comparisons with plain Nix. Every measured outer
command records monotonic wall time, distinct sampled process-tree PIDs, captured output bytes, and
peak summed process-tree RSS on Linux. Recursive `nix derivation show` supplies derivation counts;
exact graph output paths queried with `nix path-info` supply cache reuse; changing one dependency and
comparing target derivation paths supplies invalidation fan-out. Each implementation gets a distinct
recorded run ID so realization is not accidentally a no-op, then is run again unchanged to measure a
no-op rebuild. A selected-check sample separately exercises the CLI's explicit discovery and
selection path, and the engine sample also covers app-run. A small auxiliary suite records CLI
startup, deterministic planning, and SIGTERM cancellation while nix-tools is building a generated
long-running derivation. Cancellation waits for the streamed `realizing` event and an observable Nix
child, then requires both nix-tools and that child to terminate within bounded deadlines. Pass a built
`--bun2nix` binary to include offline lockfile inspection and local-only
parse/render measurements. External-source prefetch is deliberately not benchmarked offline because
realistic sources require network/cache state; supply `--bun2nix-external-lockfile PATH` to opt into
a real prefetch/convert run. Cross-language protocol overhead is recorded as unavailable until that
protocol exists.

Run the benchmark from a clean development shell:

```sh
cargo build --release --package nix-tools
PYTHONDONTWRITEBYTECODE=1 python3 benchmarks/benchmark.py \
  --run-id "$(date -u +%Y%m%dT%H%M%SZ)" \
  --repeats 5 \
  --bun2nix target/release/bun2nix \
  --output benchmarks/results/latest.json
```

Pass `--nix-fast-build /path/to/nix-fast-build` to force that optional comparator. The harness uses
the upstream CLI's complete `#checks.<system>` flake path and non-interactive renderer. One invocation
produces a distribution per scenario and implementation. Use at least five repeats for exploratory
runs and more for regression thresholds; p95 from very small samples is descriptive, not inferential.
Process counts are sampled every 10 ms, so very short-lived descendants may be missed, and Linux RSS
is a sampled peak rather than an allocator profile. Run on an otherwise idle machine with stable Nix
daemon/cache settings, and compare distributions from the same host rather than isolated p50 values.

Remote-cache behavior is opt-in so an offline benchmark never performs hidden network work. Supply
`--remote-cache-url URL`, `--remote-cache-public-key KEY`, `--remote-cache-flake FLAKE`,
`--remote-cache-package PACKAGE`, and `--remote-cache-store-root PATH` together. For every repeat the
harness resolves the known package output, creates a distinct empty local store, proves the output is
absent there, proves the supplied cache advertises it, then measures nix-tools against that isolated
store and records those preconditions separately. External Bun conversion similarly requires both
`--bun2nix` and `--bun2nix-external-lockfile`.

## Process, allocation, and application handoff comparison

The 2026-09-08 comparison uses release builds on the same Linux x86_64 host: baseline
`1a2205c8` and the working-tree optimization implementation. Three independent runs per
workload measure wall time with a monotonic clock and CPU/RSS with `wait4` resource usage.
Separate `heaptrack` runs measure allocation calls, total allocated bytes (the exact size/count
histogram), and peak live heap; separate `strace -f -c` runs count syscalls. Profiling overhead
is excluded from timings. CPU and syscall counts include descendants; allocator data describes
the instrumented application process, with instrumentation ending at an environment-clearing
exec. Peak heap is heaptrack's rounded summary, not RSS. These are synthetic mechanism
comparisons, not forecasts of an entire Nix build. Three-sample medians are exploratory.

The workloads are checked in: `process_bench` exercises 1,000 short captured or relayed
children and twenty 250 ms idle children (relay uses the runner's known nonblocking discard
sink, not a terminal or arbitrary callback); `graph_load --bench` streams 3,757 nodes fifteen
times and retains two progress/manifest snapshots; `probe_bench` makes ten 10,000-root
cache-hit requests with a 728 KiB probe result; `app_execution` hands 512 MiB from `dd` to
`/dev/null`; generated Bun fixtures inspect 200 workspaces sharing 150 packages, and convert
64 package entries sharing eight source URLs. The prefetch fixture uses an offline fake Nix
with a deterministic 10 ms delay, so the prefetch result isolates deduplication/concurrency
and makes no network performance claim. The graph parser was already typed and streaming
at baseline; that row measures shared graph payload ownership.

<!-- optimization-results:start -->
| Workload | CPU seconds | Wall seconds | Allocation calls | Allocated MB | Peak heap MB | Syscalls |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| process-throughput | 0.2379 → 0.2034 | 0.3048 → 0.1933 | 38,022 → 18,015 | 11.09 → 0.85 | 0.09 → 0.08 | 115,104 → 89,082 |
| process-idle | 0.0348 → 0.0188 | 5.0908 → 5.0193 | 778 → 355 | 0.30 → 0.09 | 0.09 → 0.08 | 6,344 → 2,718 |
| process-relay | 0.2411 → 0.1909 | 0.3121 → 0.1821 | 49,023 → 41,015 | 18.32 → 1.15 | 0.09 → 0.08 | 96,106 → 85,085 |
| graph-stream | 0.5783 → 0.4159 | 0.5785 → 0.4161 | 5,853,403 → 2,761,633 | 717.86 → 311.87 | 43.93 → 17.22 | 29,610 → 26,275 |
| probe | 0.2978 → 0.2979 | 0.2977 → 0.3189 | 5,196,452 → 5,396,452 | 1026.98 → 1022.38 | 73.55 → 72.91 | 7,723 → 7,262 |
| bun-closures | 0.1302 → 0.0464 | 0.1303 → 0.0464 | 2,389,697 → 958,340 | 205.30 → 23.34 | 5.29 → 5.29 | 149 → 150 |
| bun-prefetch | 0.0929 → 0.0127 | 0.7347 → 0.0239 | 3,141 → 2,334 | 0.34 → 0.30 | 0.15 → 0.15 | 20,635 → 2,827 |
| app | 0.9336 → 0.0056 | 0.9091 → 0.0057 | 81 → 40 | 0.11 → 0.09 | 0.10 → 0.08 | 279,082 → 16,533 |

All cells show before → after medians. Allocated MB is cumulative allocation traffic, while
peak heap MB is simultaneously live memory. Decimal MB is used throughout.

| Cancellation workload | Median milliseconds | Remaining groups after parent exit |
| --- | ---: | --- |
| process | 13.226 → 2.328 | token-to-runner-return; no outer exit |
| process-relay | 13.385 → 2.450 | token-to-runner-return; no outer exit |
| process-default | 13.289 → 2.064 | token-to-runner-return; no outer exit |
| app | 63.358 → 1.076 | 0/20 → 0/20 |
| bun-prefetch | 1.074 → 1.079 | 20/20 → 20/20 |
<!-- optimization-results:end -->

Graph snapshotting cuts allocation traffic by 57%; shared Bun traversal cuts it by 89%.
The final probe workload also emits cached-job graph and completion progress. It removes
4.6 MB of allocation traffic, while shared ownership and those required progress events
increase allocation calls by 3.8%. CPU time is unchanged; wall samples overlap
(before 0.295–0.311 s, after 0.292–0.408 s), so this row supports no latency improvement claim. Silenced relay now reduces allocation calls and syscalls as well as
wall time; its nonblocking sink does not exercise arbitrary callback isolation. Prefetch peak heap is slightly higher with four concurrent workers.

Cancellation samples use twenty independent requests. Process modes request SIGINT through
the token after 50 ms; `process` and `process-relay` set a 20 ms cleanup bound, while
`process-default` retains the public two-second default. App and Bun samples send SIGTERM
after 50 ms and wait for outer-process exit (Python polling adds roughly millisecond resolution).
Graph ownership and probe metric transfer are synchronous, non-cancellable operations; their
cancellation result is not applicable, and process cancellation is measured separately. Bun
prefetch still has no cooperative cancellation contract: both versions leave prefetch children
after the parent exits. The cancellation harness detects and kills that remaining process group;
fast parent termination must not be read as successful descendant cleanup.


Reproduce a workload with the same example/benchmark source copied to the baseline checkout:

```sh
cargo build --release --workspace --examples --benches
cargo build --release --workspace
python3 benchmarks/optimization_fixtures.py benchmarks/results
python3 benchmarks/profile_command.py --repeats 3 \
  --output benchmarks/results/process-throughput-after.json -- \
  target/release/examples/process_bench throughput
NIX_TOOLS_GRAPH_MODE=stream python3 benchmarks/profile_command.py --repeats 3 \
  --output benchmarks/results/graph-stream-after.json -- \
  target/release/deps/graph_load-<hash> --bench
PATH="$PWD/benchmarks/results/fake-bin:$PATH" python3 benchmarks/profile_command.py \
  --repeats 3 --output benchmarks/results/bun-prefetch-after.json -- \
  target/release/bun2nix convert -l benchmarks/results/bun-prefetch.lock -o /dev/null
target/release/examples/process_bench cancel-default > benchmarks/results/process-default-cancel-after.txt
python3 benchmarks/cancellation_profile.py --repeats 20 \
  --output benchmarks/results/app-cancel-after.json -- \
  target/release/examples/app_execution exec wait
```

The tools must be installed and their loader dependencies resolvable. On this host, system
heaptrack required a library search directory containing `libunwind.so.8`, `libstdc++.so.6`,
and `liblzma.so.5` when instrumenting Nix-linked Rust binaries; an uninstrumented run is
never substituted for a failed allocation measurement. `benchmarks/optimization-results.json`
retains the compact measured summary; raw profiles and samples remain ignored under
`benchmarks/results/`. The Python parsers reject missing profiler summaries and failed commands.

## Root-only realization result

Two consecutive three-sample release runs on 2026-08-27 compare the batched recursive-graph engine
with the root-only success path. The final engine retains the detailed graph/cache path for one root,
where it is faster, and for failed batches, where dependency diagnostics matter. Successful requests
with two or more roots perform one evaluation, one offline root probe, and one root-only build. Raw
results are `benchmarks/results/post-batch-baseline.json`,
`benchmarks/results/root-fast-optimized.json`, and the five-sample one-root confirmation in
`benchmarks/results/single-root-hybrid.json`; all are intentionally ignored.

| Targets | Dependencies | Before p50 / p95 (s) | Final p50 / p95 (s) | p50 change | Processes |
| ---: | --- | ---: | ---: | ---: | ---: |
| 1 | shared | 0.887 / 0.951 | 0.905 / 0.965 | +2.0% | 6 → 6 |
| 1 | exclusive | 0.955 / 0.989 | 0.912 / 0.945 | -4.4% | 6 → 6 |
| 8 | shared | 2.697 / 2.910 | 1.587 / 1.692 | -41.1% | 6 → 4 |
| 8 | exclusive | 2.737 / 3.821 | 2.024 / 4.628 | -26.0% | 6 → 4 |
| 32 | shared | 7.610 / 8.354 | 2.586 / 2.823 | -66.0% | 6 → 4 |
| 32 | exclusive | 8.358 / 8.640 | 3.511 / 3.655 | -58.0% | 6 → 4 |
| 128 | shared | 27.271 / 27.525 | 3.091 / 3.470 | -88.7% | 6 → 4 |
| 128 | exclusive | 29.446 / 30.148 | 7.706 / 9.897 | -73.8% | 6 → 4 |

At 128 targets, removing the redundant full-closure cache probe cut its attributed child time from
about 23.3 seconds to 0.12 seconds and removed recursive graph construction from successful runs.
The 128-target shared engine is about 7.2 times faster than plain Nix in the same optimized run
(3.091 versus 22.147 seconds p50). The eight-target exclusive p95 contains one noisy slow sample;
its p50 and every 32/128 p50 and p95 show the architectural reduction. The five-sample one-root
confirmation removed the regression seen when the root-only path was applied indiscriminately.

## Historical single-sample baseline

The following corrected single-sample run measures the release `nix-tools` binary against real Nix;
only the separate attribution run uses the tracing wrapper. It used the working tree on 2026-08-26,
Nix 2.34.8, x86_64 Linux 7.0, and nixpkgs revision
`56c02bc00adcf003215cc4bd996d6efaf4cff188`. RSS is MiB. Raw output is
`benchmarks/results/corrected-unwrapped-2026-08-26.json` and is intentionally ignored.

| Targets | Dependencies | Derivations | Invalidation | `nix-tools` realize / no-op (s) | `nix-tools` processes / RSS | Plain Nix realize / no-op (s) | Plain Nix processes / RSS |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | shared | 574 | 1 | 0.869 / 0.239 | 6 / 107.6 | 0.279 / 0.377 | 1 / 103.7 |
| 1 | exclusive | 574 | 1 | 0.932 / 0.463 | 7 / 109.2 | 0.379 / 0.376 | 1 / 105.4 |
| 8 | shared | 581 | 8 | 2.162 / 0.219 | 12 / 155.9 | 1.283 / 1.055 | 1 / 435.8 |
| 8 | exclusive | 588 | 1 | 2.418 / 0.219 | 12 / 152.9 | 3.818 / 1.129 | 1 / 435.0 |
| 32 | shared | 605 | 32 | 7.396 / 0.460 | 29 / 158.9 | 10.942 / 3.941 | 1 / 1574.7 |
| 32 | exclusive | 636 | 1 | 7.269 / 0.420 | 30 / 159.9 | 6.106 / 5.137 | 1 / 1576.6 |
| 128 | shared | 701 | 128 | 47.525 / 0.462 | 105 / 413.7 | 18.002 / 15.902 | 1 / 5173.4 |
| 128 | exclusive | 828 | 1 | 29.776 / 0.590 | 105 / 424.1 | 20.006 / 15.849 | 1 / 5183.5 |

For the completed samples, graph cache validity moved from 479 to 481 paths at one target, from
481 to 488 (shared) or 495 (exclusive) at eight targets, from 488 to 512 (shared) or 495 to 543
(exclusive) at 32 targets, and from 512 to 608 (shared) or 543 to 735 (exclusive) at 128 targets.
Captured output peaked at 16.9 KiB for `nix-tools` and 1.67 MiB for plain Nix. The engine's warm no-op
stayed between 0.219 and 0.590 seconds while plain Nix rose to about 15.9 seconds at 128 targets. Cold
realization was faster for the eight-target exclusive and 32-target shared samples, and slower for
the other six. This supports the warm-root fast path, but not further production optimization from
this single sample.

`nix-fast-build` was not installed, so it has no fabricated comparison row.

## Legacy traced implementation deltas

The ignored `dependency-scheduler-baseline-2026-08-26.json` used the same worktree and harness
structure, but disabled the local-root fast path and restored the old per-dependency scheduler. Its
one-target cold realization results isolate the cost of traversing and individually submitting the
recursive graph. Both sides of this historical comparison include the same tracing-wrapper overhead;
they remain internally comparable, but are not comparable with a corrected unwrapped harness run:

| Dependencies | Old scheduler wall / Nix builds / OS processes | Legacy traced current wall / Nix builds / OS processes | Wall reduction |
| --- | ---: | ---: | ---: |
| shared | 92.030 s / 574 / 1,098 | 1.015 s / 1 / 13 | 98.9% (90.7x) |
| exclusive | 15.791 s / 574 / 1,068 | 1.480 s / 1 / 13 | 90.6% (10.7x) |

The ignored `no-fast-path-2026-08-26.json` changed only the warm local-root fast path. Comparing its
no-op wall time with the same legacy traced current sample shows the following deltas:

| Targets | Dependencies | Without fast path | Legacy traced current | Change |
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
