#!/usr/bin/env python3
"""Reproducible nix-tools monorepo benchmark harness."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import json
import os
import platform
import shutil
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Sequence


TARGET_COUNTS = (1, 8, 32, 128)
DEPENDENCY_SHAPES = ("shared", "exclusive")


@dataclasses.dataclass(frozen=True)
class Scenario:
    targets: int
    dependency_shape: str

    def __post_init__(self) -> None:
        if self.targets < 1:
            raise ValueError("target count must be positive")
        if self.dependency_shape not in DEPENDENCY_SHAPES:
            raise ValueError(f"unknown dependency shape: {self.dependency_shape}")

    @property
    def name(self) -> str:
        return f"{self.targets:03d}-{self.dependency_shape}"

    def as_dict(self) -> dict[str, int | str]:
        return dataclasses.asdict(self)


@dataclasses.dataclass(frozen=True)
class Measurement:
    wall_seconds: float
    process_count: int
    peak_rss_bytes: int | None
    retained_output_bytes: int
    exit_code: int
    stdout: bytes = dataclasses.field(repr=False)
    stderr: bytes = dataclasses.field(repr=False)

    def as_dict(self) -> dict[str, float | int | None]:
        return {
            "wall_seconds": round(self.wall_seconds, 6),
            "process_count": self.process_count,
            "peak_rss_bytes": self.peak_rss_bytes,
            "retained_output_bytes": self.retained_output_bytes,
            "exit_code": self.exit_code,
        }


def scenarios(target_counts: Sequence[int] = TARGET_COUNTS) -> list[Scenario]:
    return [Scenario(count, shape) for count in target_counts for shape in DEPENDENCY_SHAPES]


def render_flake(
    scenario: Scenario,
    *,
    system: str,
    nixpkgs_url: str,
    mutation: int,
    salt: str = "baseline",
) -> str:
    bindings: list[str] = []
    packages: list[str] = []
    if scenario.dependency_shape == "shared":
        bindings.append(
            f'''sharedDependency = pkgs.runCommand "benchmark-shared-dependency" {{ }} ''\n'''
            f'''  printf '%s\\n' 'mutation-{mutation}-{salt}' > "$out"\n'''
            "'';"
        )
    for index in range(scenario.targets):
        suffix = f"{index:03d}"
        if scenario.dependency_shape == "exclusive":
            marker = f"mutation-{mutation}" if index == 0 else "stable-exclusive"
            bindings.append(
                f'''exclusiveDependency{suffix} = pkgs.runCommand "benchmark-exclusive-{suffix}" {{ }} ''\n'''
                f'''  printf '%s\\n' '{marker}-{salt}' > "$out"\n'''
                "'';"
            )
            dependency = f"exclusiveDependency{suffix}"
        else:
            dependency = "sharedDependency"
        bindings.append(
            f'''target{suffix} = pkgs.runCommand "benchmark-target-{suffix}" {{ }} ''\n'''
            f"  cat ${{{dependency}}} > \"$out\"\n"
            f'''  printf '%s\\n' '{salt}' >> "$out"\n'''
            "'';"
        )
        packages.append(f'"target-{suffix}" = target{suffix};')
    bindings_text = "\n        ".join(bindings)
    packages_text = "\n          ".join(packages)
    return f'''{{
  description = "nix-tools generated benchmark fixture";
  inputs.nixpkgs.url = "{nixpkgs_url}";
  outputs = {{ nixpkgs, ... }}:
    let
      system = "{system}";
      pkgs = import nixpkgs {{ inherit system; }};
      {bindings_text}
      benchmarkPackages = {{
          {packages_text}
      }};
    in {{
      packages.${{system}} = benchmarkPackages;
      checks.${{system}} = benchmarkPackages;
    }};
}}
'''


def classify_nix_command(command: Sequence[str]) -> str:
    words = list(command)
    if "eval" in words:
        return "evaluation"
    if "derivation" in words and "show" in words:
        return "graph_construction"
    if "path-info" in words:
        return "cache_probe"
    if "build" in words:
        return "realization"
    return "other"


def invalidation_fan_out(baseline: dict[str, str], mutated: dict[str, str]) -> int:
    if baseline.keys() != mutated.keys():
        raise ValueError("invalidation target sets differ")
    return sum(baseline[name] != mutated[name] for name in baseline)


def _linux_pid_and_parent(stat: str) -> tuple[int, int]:
    fields = stat.split()
    post_comm = stat.rsplit(")", 1)[1].split()
    return int(fields[0]), int(post_comm[1])


def _linux_process_sample(root_pid: int) -> tuple[set[int], int] | None:
    proc = Path("/proc")
    if not proc.is_dir():
        return None
    parents: dict[int, int] = {}
    rss: dict[int, int] = {}
    page_size = os.sysconf("SC_PAGE_SIZE")
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            pid, parent = _linux_pid_and_parent((entry / "stat").read_text())
            statm = (entry / "statm").read_text().split()
            parents[pid] = parent
            rss[pid] = int(statm[1]) * page_size
        except (OSError, IndexError, ValueError):
            continue
    descendants = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, parent in parents.items():
            if parent in descendants and pid not in descendants:
                descendants.add(pid)
                changed = True
    return descendants, sum(rss.get(pid, 0) for pid in descendants)


def measure(
    command: Sequence[str],
    *,
    cwd: Path,
    env: dict[str, str] | None = None,
    input_bytes: bytes | None = None,
) -> Measurement:
    started = time.monotonic()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE if input_bytes is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    observed: set[int] = set()
    peak_rss: int | None = None
    stop = threading.Event()

    def sample() -> None:
        nonlocal peak_rss
        while not stop.is_set():
            current = _linux_process_sample(process.pid)
            if current is not None:
                pids, rss = current
                observed.update(pids)
                peak_rss = max(peak_rss or 0, rss)
            stop.wait(0.01)

    sampler = threading.Thread(target=sample, daemon=True)
    sampler.start()
    stdout, stderr = process.communicate(input_bytes)
    stop.set()
    sampler.join()
    observed.add(process.pid)
    return Measurement(
        wall_seconds=time.monotonic() - started,
        process_count=len(observed),
        peak_rss_bytes=peak_rss,
        retained_output_bytes=len(stdout) + len(stderr),
        exit_code=process.returncode,
        stdout=stdout,
        stderr=stderr,
    )


def checked(measurement: Measurement, description: str) -> Measurement:
    if measurement.exit_code != 0:
        error = measurement.stderr.decode(errors="replace")[-4000:]
        raise RuntimeError(f"{description} failed ({measurement.exit_code}):\n{error}")
    return measurement


def nixpkgs_url(repo: Path) -> str:
    lock = json.loads((repo / "flake.lock").read_text())
    locked = lock["nodes"]["nixpkgs"]["locked"]
    if locked.get("type") != "github":
        raise RuntimeError("root nixpkgs input is not GitHub-pinned")
    return f"github:{locked['owner']}/{locked['repo']}/{locked['rev']}"


def target_names(scenario: Scenario) -> list[str]:
    return [f"target-{index:03d}" for index in range(scenario.targets)]


def installables(fixture: Path, scenario: Scenario, system: str) -> list[str]:
    return [f"{fixture}#packages.{system}.{name}" for name in target_names(scenario)]


def derivation_graph(nix: str, fixture: Path, scenario: Scenario, system: str) -> dict[str, Any]:
    result = checked(
        measure(
            [nix, "derivation", "show", "--recursive", *installables(fixture, scenario, system)],
            cwd=fixture,
        ),
        "derivation graph query",
    )
    return json.loads(result.stdout)


def graph_entries(graph: dict[str, Any]) -> dict[str, Any]:
    derivations = graph.get("derivations")
    return derivations if isinstance(derivations, dict) else graph


def output_paths(graph: dict[str, Any]) -> list[str]:
    store_directory = graph.get("storeDir", "/nix/store" if "derivations" in graph else None)
    prefix = f"{store_directory}/" if isinstance(store_directory, str) else ""
    return sorted(
        output["path"] if output["path"].startswith("/") else prefix + output["path"]
        for derivation in graph_entries(graph).values()
        for output in derivation.get("outputs", {}).values()
        if output.get("path")
    )


def valid_path_count(nix: str, fixture: Path, paths: Sequence[str]) -> int:
    if not paths:
        return 0
    result = measure(
        [nix, "path-info", "--json", "--stdin"],
        cwd=fixture,
        input_bytes=("\n".join(paths) + "\n").encode(),
    )
    try:
        response = json.loads(result.stdout or b"{}")
    except json.JSONDecodeError:
        return 0
    if isinstance(response, dict):
        return sum(value is not None for value in response.values())
    return len(response)


def target_derivations(nix: str, fixture: Path, system: str) -> dict[str, str]:
    result = checked(
        measure(
            [
                nix,
                "eval",
                "--json",
                f"{fixture}#packages.{system}",
                "--apply",
                "packages: builtins.mapAttrs (_: value: value.drvPath) packages",
            ],
            cwd=fixture,
        ),
        "target derivation query",
    )
    return json.loads(result.stdout)


TRACE_WRAPPER = r'''#!/usr/bin/env python3
import json
import os
import subprocess
import sys
import time

started = time.monotonic()
result = subprocess.run([__NIX_REAL__, *sys.argv[1:]])
record = {
    "argv": sys.argv[1:],
    "exit_code": result.returncode,
    "wall_seconds": time.monotonic() - started,
}
with open(__NIX_TRACE__, "a", encoding="utf-8") as trace:
    trace.write(json.dumps(record, sort_keys=True) + "\n")
raise SystemExit(result.returncode)
'''


def write_fixture(
    directory: Path,
    scenario: Scenario,
    *,
    system: str,
    pinned_nixpkgs: str,
    mutation: int,
    salt: str,
    nix: str,
) -> None:
    directory.mkdir(parents=True)
    (directory / "flake.nix").write_text(
        render_flake(
            scenario,
            system=system,
            nixpkgs_url=pinned_nixpkgs,
            mutation=mutation,
            salt=salt,
        )
    )
    checked(measure([nix, "flake", "lock"], cwd=directory), "fixture lock")


def aggregate_trace(trace_file: Path) -> dict[str, dict[str, float | int]]:
    totals: dict[str, dict[str, float | int]] = {}
    if not trace_file.exists():
        return totals
    for line in trace_file.read_text().splitlines():
        record = json.loads(line)
        phase = classify_nix_command(["nix", *record["argv"]])
        total = totals.setdefault(phase, {"invocations": 0, "child_wall_seconds": 0.0})
        total["invocations"] = int(total["invocations"]) + 1
        total["child_wall_seconds"] = round(
            float(total["child_wall_seconds"]) + float(record["wall_seconds"]), 6
        )
    return totals


def benchmark_plain(
    nix: str,
    fixture: Path,
    scenario: Scenario,
    system: str,
) -> dict[str, Any]:
    targets = installables(fixture, scenario, system)
    evaluation = checked(
        measure(
            [
                nix,
                "eval",
                "--json",
                f"{fixture}#packages.{system}",
                "--apply",
                "builtins.attrNames",
            ],
            cwd=fixture,
        ),
        "plain Nix evaluation",
    )
    graph = checked(
        measure([nix, "derivation", "show", "--recursive", *targets], cwd=fixture),
        "plain Nix graph construction",
    )
    graph_json = json.loads(graph.stdout)
    paths = output_paths(graph_json)
    cache_before = valid_path_count(nix, fixture, paths)
    cache_probe = measure(
        [nix, "path-info", "--json", "--stdin"],
        cwd=fixture,
        input_bytes=("\n".join(paths) + "\n").encode(),
    )
    realization = checked(
        measure([nix, "build", "--no-link", *targets], cwd=fixture), "plain Nix realization"
    )
    cache_after = valid_path_count(nix, fixture, paths)
    no_op = checked(
        measure([nix, "build", "--no-link", *targets], cwd=fixture), "plain Nix no-op build"
    )
    return {
        "phases": {
            "evaluation": evaluation.as_dict(),
            "graph_construction": graph.as_dict(),
            "cache_probe": cache_probe.as_dict(),
            "realization": realization.as_dict(),
            "no_op_rebuild": no_op.as_dict(),
        },
        "derivation_count": len(graph_entries(graph_json)),
        "cache_reuse": {
            "valid_paths_before": cache_before,
            "valid_paths_after": cache_after,
            "graph_output_paths": len(paths),
        },
    }


def benchmark_engine(
    nix: str,
    engine: Path,
    fixture: Path,
    trace_fixture: Path,
    scenario: Scenario,
    system: str,
    trace_wrapper: Path,
) -> dict[str, Any]:
    graph = derivation_graph(nix, fixture, scenario, system)
    paths = output_paths(graph)
    trace_file = trace_fixture / "engine-trace.jsonl"
    trace_wrapper.write_text(
        TRACE_WRAPPER.replace("__NIX_REAL__", repr(nix)).replace(
            "__NIX_TRACE__", repr(str(trace_file))
        )
    )
    trace_wrapper.chmod(0o755)
    cache_before = valid_path_count(nix, fixture, paths)
    checked(
        measure(
            [
                str(engine),
                "--nix",
                str(trace_wrapper),
                "build",
                "--flake",
                str(trace_fixture),
            ],
            cwd=trace_fixture,
        ),
        "traced nix-tools realization",
    )
    realization_trace = aggregate_trace(trace_file)
    trace_file.unlink(missing_ok=True)
    checked(
        measure(
            [
                str(engine),
                "--nix",
                str(trace_wrapper),
                "build",
                "--flake",
                str(trace_fixture),
            ],
            cwd=trace_fixture,
        ),
        "traced nix-tools no-op build",
    )
    no_op_trace = aggregate_trace(trace_file)
    realization = checked(
        measure(
            [str(engine), "--nix", nix, "build", "--flake", str(fixture)],
            cwd=fixture,
        ),
        "nix-tools realization",
    )
    cache_after = valid_path_count(nix, fixture, paths)
    no_op = checked(
        measure(
            [str(engine), "--nix", nix, "build", "--flake", str(fixture)],
            cwd=fixture,
        ),
        "nix-tools no-op build",
    )
    return {
        "phases": {
            "realization": realization.as_dict(),
            "no_op_rebuild": no_op.as_dict(),
        },
        "engine_nix_subprocesses": {
            "realization": realization_trace,
            "no_op_rebuild": no_op_trace,
        },
        "derivation_count": len(graph_entries(graph)),
        "cache_reuse": {
            "valid_paths_before": cache_before,
            "valid_paths_after": cache_after,
            "graph_output_paths": len(paths),
        },
    }


def benchmark_fast_build(
    executable: str,
    fixture: Path,
    system: str,
) -> dict[str, Any]:
    flake = f"{fixture}#checks.{system}"
    first = checked(
        measure([executable, "--flake", flake, "--no-nom"], cwd=fixture),
        "nix-fast-build realization",
    )
    second = checked(
        measure([executable, "--flake", flake, "--no-nom"], cwd=fixture),
        "nix-fast-build no-op build",
    )
    return {
        "phases": {"realization": first.as_dict(), "no_op_rebuild": second.as_dict()},
        "derivation_count": None,
        "cache_reuse": None,
    }


def benchmark_scenario(
    root: Path,
    scenario: Scenario,
    *,
    nix: str,
    engine: Path,
    fast_build: str | None,
    system: str,
    pinned_nixpkgs: str,
    trace_wrapper: Path,
    run_id: str,
) -> dict[str, Any]:
    scenario_root = root / scenario.name
    implementations: dict[str, Any] = {}
    for implementation in ("nix-tools", "plain-nix"):
        fixture = scenario_root / implementation
        write_fixture(
            fixture,
            scenario,
            system=system,
            pinned_nixpkgs=pinned_nixpkgs,
            mutation=0,
            salt=f"{implementation}-{run_id}",
            nix=nix,
        )
        if implementation == "nix-tools":
            trace_fixture = scenario_root / "nix-tools-trace"
            write_fixture(
                trace_fixture,
                scenario,
                system=system,
                pinned_nixpkgs=pinned_nixpkgs,
                mutation=0,
                salt=f"nix-tools-trace-{run_id}",
                nix=nix,
            )
            implementations[implementation] = benchmark_engine(
                nix, engine, fixture, trace_fixture, scenario, system, trace_wrapper
            )
        else:
            implementations[implementation] = benchmark_plain(nix, fixture, scenario, system)
    if fast_build:
        fixture = scenario_root / "nix-fast-build"
        write_fixture(
            fixture,
            scenario,
            system=system,
            pinned_nixpkgs=pinned_nixpkgs,
            mutation=0,
            salt=f"nix-fast-build-{run_id}",
            nix=nix,
        )
        implementations["nix-fast-build"] = benchmark_fast_build(fast_build, fixture, system)
    baseline = scenario_root / "invalidation-baseline"
    mutated = scenario_root / "invalidation-mutated"
    for directory, mutation in ((baseline, 0), (mutated, 1)):
        write_fixture(
            directory,
            scenario,
            system=system,
            pinned_nixpkgs=pinned_nixpkgs,
            mutation=mutation,
            salt="invalidation",
            nix=nix,
        )
    fan_out = invalidation_fan_out(
        target_derivations(nix, baseline, system), target_derivations(nix, mutated, system)
    )
    return {
        "scenario": scenario.as_dict(),
        "invalidation_fan_out": fan_out,
        "implementations": implementations,
    }


def detect_system(nix: str, repo: Path) -> str:
    result = checked(measure([nix, "eval", "--raw", "--impure", "--expr", "builtins.currentSystem"], cwd=repo), "Nix system detection")
    return result.stdout.decode().strip()


def version(command: Sequence[str], cwd: Path) -> str:
    result = subprocess.run(command, cwd=cwd, capture_output=True, text=True, check=False)
    return (result.stdout or result.stderr).strip().splitlines()[0]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--nix", default=shutil.which("nix") or "nix")
    parser.add_argument("--engine", type=Path)
    parser.add_argument("--nix-fast-build", default=shutil.which("nix-fast-build"))
    parser.add_argument("--targets", type=int, nargs="+", default=list(TARGET_COUNTS))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--run-id", default=str(time.time_ns()))
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    repo = args.repo.resolve()
    engine = (args.engine or repo / "target" / "release" / "nix-tools").resolve()
    if not engine.is_file():
        raise SystemExit(f"missing benchmark engine {engine}; run cargo build --release -p nix-tools")
    output = args.output or repo / "benchmarks" / "results" / "latest.json"
    output = output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    system = detect_system(args.nix, repo)
    pinned_nixpkgs = nixpkgs_url(repo)
    with tempfile.TemporaryDirectory(prefix="nix-tools-benchmark-") as temporary:
        temporary_root = Path(temporary)
        trace_wrapper = temporary_root / "trace-nix"
        results = []
        document = {
            "schema_version": 1,
            "complete": False,
            "recorded_at": dt.datetime.now(dt.UTC).isoformat(),
            "environment": {
                "system": system,
                "platform": platform.platform(),
                "nix": version([args.nix, "--version"], repo),
                "nix_tools_binary": str(engine),
                "nix_fast_build": version([args.nix_fast_build, "--version"], repo)
                if args.nix_fast_build
                else None,
                "nixpkgs": pinned_nixpkgs,
                "run_id": args.run_id,
            },
            "metrics": {
                "wall_seconds": "monotonic elapsed time",
                "process_count": "distinct sampled process-tree PIDs",
                "peak_rss_bytes": "peak summed Linux process-tree RSS; null elsewhere",
                "retained_output_bytes": "captured stdout plus stderr bytes",
                "derivation_count": "recursive nix derivation show entries",
                "cache_reuse": "valid graph output paths before and after realization",
                "invalidation_fan_out": "target derivation paths changed by one dependency mutation",
            },
            "results": results,
        }
        output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
        for scenario in scenarios(args.targets):
            results.append(benchmark_scenario(
                temporary_root,
                scenario,
                nix=args.nix,
                engine=engine,
                fast_build=args.nix_fast_build,
                system=system,
                pinned_nixpkgs=pinned_nixpkgs,
                trace_wrapper=trace_wrapper,
                run_id=args.run_id,
            ))
            output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    document["complete"] = True
    output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    print(output)


if __name__ == "__main__":
    main()
