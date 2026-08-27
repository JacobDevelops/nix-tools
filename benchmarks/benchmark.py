#!/usr/bin/env python3
"""Reproducible nix-tools monorepo benchmark harness."""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import json
import os
import platform
import selectors
import shlex
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


def percentile(values: Sequence[float | int], quantile: float) -> float:
    if not values:
        raise ValueError("percentile requires at least one value")
    if not 0 <= quantile <= 1:
        raise ValueError("quantile must be between zero and one")
    ordered = sorted(float(value) for value in values)
    position = (len(ordered) - 1) * quantile
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    return round(
        ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower), 12
    )


def measurement_summary(samples: Sequence[Measurement]) -> dict[str, Any]:
    if not samples:
        raise ValueError("summary requires at least one sample")

    def distribution(values: Sequence[float | int]) -> dict[str, float | int]:
        return {
            "min": min(values),
            "p50": round(percentile(values, 0.5), 6),
            "p95": round(percentile(values, 0.95), 6),
            "max": max(values),
        }

    rss = [sample.peak_rss_bytes for sample in samples if sample.peak_rss_bytes is not None]
    return {
        "sample_count": len(samples),
        "wall_seconds": distribution([sample.wall_seconds for sample in samples]),
        "process_count": distribution([sample.process_count for sample in samples]),
        "peak_rss_bytes": distribution(rss) if rss else None,
        "retained_output_bytes": distribution(
            [sample.retained_output_bytes for sample in samples]
        ),
        "exit_codes": sorted({sample.exit_code for sample in samples}),
    }


def repeat_measure(
    command: Sequence[str],
    *,
    cwd: Path,
    repeats: int,
    env: dict[str, str] | None = None,
    input_bytes: bytes | None = None,
) -> dict[str, Any]:
    if repeats < 1:
        raise ValueError("repeats must be positive")
    samples = [
        checked(
            measure(command, cwd=cwd, env=env, input_bytes=input_bytes),
            f"benchmark command {' '.join(command)}",
        )
        for _ in range(repeats)
    ]
    return {
        "command": list(command),
        "samples": [sample.as_dict() for sample in samples],
        "summary": measurement_summary(samples),
    }


def _summarize_values(values: Sequence[Any]) -> Any:
    if all(
        isinstance(value, (int, float)) and not isinstance(value, bool) for value in values
    ):
        return {
            "min": min(values),
            "p50": round(percentile(values, 0.5), 6),
            "p95": round(percentile(values, 0.95), 6),
            "max": max(values),
        }
    if all(isinstance(value, dict) for value in values):
        common_keys = set(values[0]).intersection(*(set(value) for value in values[1:]))
        return {
            key: _summarize_values([value[key] for value in values])
            for key in sorted(common_keys)
        }
    return values[0] if all(value == values[0] for value in values) else list(values)


def summarize_scenario_samples(samples: Sequence[dict[str, Any]]) -> dict[str, Any]:
    if not samples:
        raise ValueError("scenario summary requires at least one sample")
    return {
        "scenario": samples[0]["scenario"],
        "sample_count": len(samples),
        "samples": list(samples),
        "summary": _summarize_values(samples),
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
    apps_text = "\n          ".join(
        f'"target-{index:03d}" = {{ type = "app"; program = "${{pkgs.writeShellScript "benchmark-app-{index:03d}" "exit 0"}}"; }};'
        for index in range(scenario.targets)
    )
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
      apps.${{system}} = {{
          {apps_text}
      }};
    }};
}}
'''


def render_cancellation_flake(*, system: str, nixpkgs_url: str, salt: str) -> str:
    return f'''{{
  inputs.nixpkgs.url = "{nixpkgs_url}";
  outputs = {{ nixpkgs, ... }}:
    let pkgs = import nixpkgs {{ system = "{system}"; }};
    in {{
      packages.{system}.cancel = pkgs.runCommand "benchmark-cancel-{salt}" {{ }} ''
        sleep 60
        touch "$out"
      '';
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


def _process_descendants(root_pid: int) -> set[int]:
    linux = _linux_process_sample(root_pid)
    if linux is not None:
        return linux[0]
    listing = subprocess.run(
        ["ps", "-eo", "pid=,ppid="], capture_output=True, text=True, check=False
    )
    parents = {
        int(pid): int(parent)
        for line in listing.stdout.splitlines()
        if len(parts := line.split()) == 2
        for pid, parent in [parts]
    }
    descendants = {root_pid}
    while additions := {
        pid for pid, parent in parents.items() if parent in descendants and pid not in descendants
    }:
        descendants.update(additions)
    return descendants


def _pid_exists(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


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


def expected_package_output(nix: str, repo: Path, flake: str, system: str, package: str) -> str:
    result = checked(
        measure(
            [nix, "eval", "--raw", f"{flake}#packages.{system}.{package}.outPath"],
            cwd=repo,
        ),
        "remote-cache expected output resolution",
    )
    return result.stdout.decode().strip()


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
    selected_check = checked(
        measure(
            [
                str(engine),
                "--nix",
                nix,
                "check",
                "--flake",
                str(fixture),
                "--output",
                "stream",
                "target:000",
            ],
            cwd=fixture,
        ),
        "nix-tools selected check discovery",
    )
    run = checked(
        measure(
            [
                str(engine),
                "--nix",
                nix,
                "run",
                "--flake",
                str(fixture),
                "--output",
                "stream",
                "target-000",
            ],
            cwd=fixture,
        ),
        "nix-tools run",
    )
    return {
        "phases": {
            "realization": realization.as_dict(),
            "no_op_rebuild": no_op.as_dict(),
            "selected_check_discovery": selected_check.as_dict(),
            "run": run.as_dict(),
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


def measure_cancellation(
    command: Sequence[str], *, cwd: Path, start_timeout_seconds: float = 30.0
) -> Measurement:
    started = time.monotonic()
    process = subprocess.Popen(
        command,
        cwd=cwd,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    selector = selectors.DefaultSelector()
    assert process.stdout is not None and process.stderr is not None
    selector.register(process.stdout, selectors.EVENT_READ, "stdout")
    selector.register(process.stderr, selectors.EVENT_READ, "stderr")
    captured = {"stdout": bytearray(), "stderr": bytearray()}
    deadline = time.monotonic() + start_timeout_seconds
    marker = b"nix-tools: realizing "
    while marker not in captured["stderr"] and time.monotonic() < deadline:
        for key, _ in selector.select(timeout=0.1):
            chunk = os.read(key.fileobj.fileno(), 65_536)
            if chunk:
                captured[key.data].extend(chunk)
            else:
                selector.unregister(key.fileobj)
        if process.poll() is not None:
            break
    if marker not in captured["stderr"]:
        process.kill()
        process.communicate()
        raise RuntimeError("nix-tools never reported realization start")
    descendants: set[int] = {process.pid}
    descendant_deadline = time.monotonic() + 2.0
    while len(descendants) == 1 and time.monotonic() < descendant_deadline:
        descendants = _process_descendants(process.pid)
        time.sleep(0.01)
    if len(descendants) == 1:
        process.kill()
        process.communicate()
        raise RuntimeError("realization started without an observable Nix child")
    process.terminate()
    try:
        stdout, stderr = process.communicate(timeout=10)
    except subprocess.TimeoutExpired as error:
        process.kill()
        process.communicate()
        raise RuntimeError("nix-tools did not settle cancellation within 10 seconds") from error
    captured["stdout"].extend(stdout)
    captured["stderr"].extend(stderr)
    remaining = [pid for pid in descendants if pid != process.pid and _pid_exists(pid)]
    termination_deadline = time.monotonic() + 2.0
    while remaining and time.monotonic() < termination_deadline:
        time.sleep(0.01)
        remaining = [pid for pid in remaining if _pid_exists(pid)]
    if remaining:
        raise RuntimeError(f"cancellation left descendant processes running: {remaining}")
    stdout = bytes(captured["stdout"])
    stderr = bytes(captured["stderr"])
    return Measurement(
        wall_seconds=time.monotonic() - started,
        process_count=len(descendants),
        peak_rss_bytes=None,
        retained_output_bytes=len(stdout) + len(stderr),
        exit_code=process.returncode,
        stdout=stdout,
        stderr=stderr,
    )


def validate_optional_group(
    values: Sequence[str | Path | None], description: str
) -> tuple[str | Path, ...] | None:
    supplied = [value is not None for value in values]
    if any(supplied) and not all(supplied):
        raise ValueError(f"{description} arguments must be supplied together")
    return tuple(value for value in values if value is not None) if all(supplied) else None


def benchmark_auxiliary_operations(
    *,
    repo: Path,
    engine: Path,
    nix: str,
    repeats: int,
    bun2nix: Path | None,
    bun2nix_external_lockfile: Path | None,
    root: Path,
    system: str,
    pinned_nixpkgs: str,
    remote_cache: tuple[str, str, str, str, Path] | None,
) -> dict[str, Any]:
    plan_input = repo / "benchmarks" / "plan-input.json"
    operations = {
        "cli_startup": repeat_measure([str(engine), "--version"], cwd=repo, repeats=repeats),
        "plan": repeat_measure([str(engine), "plan", str(plan_input)], cwd=repo, repeats=repeats),
    }
    cancellation_samples = []
    for repeat in range(repeats):
        cancellation_fixture = root / f"cancellation-{repeat:03d}"
        cancellation_fixture.mkdir(parents=True)
        (cancellation_fixture / "flake.nix").write_text(
            render_cancellation_flake(
                system=system,
                nixpkgs_url=pinned_nixpkgs,
                salt=f"{time.time_ns()}-{repeat}",
            )
        )
        checked(
            measure([nix, "flake", "lock"], cwd=cancellation_fixture),
            "cancellation fixture lock",
        )
        sample = measure_cancellation(
            [
                str(engine),
                "--nix",
                nix,
                "build",
                "--flake",
                str(cancellation_fixture),
                "cancel",
                "--output",
                "stream",
            ],
            cwd=cancellation_fixture,
        )
        if sample.exit_code not in (-15, 143):
            raise RuntimeError(
                f"nix-tools cancellation returned {sample.exit_code}, expected SIGTERM status"
            )
        cancellation_samples.append(sample)
    operations["process_cancellation"] = {
        "samples": [sample.as_dict() for sample in cancellation_samples],
        "summary": measurement_summary(cancellation_samples),
        "scope": "SIGTERM cancellation of nix-tools during a generated long-running Nix derivation",
    }
    if remote_cache is not None:
        url, public_key, flake, package, store_root = remote_cache
        remote_samples = []
        verified_hits = []
        for repeat in range(repeats):
            output = expected_package_output(nix, repo, flake, system, package)
            isolated_root = store_root / f"repeat-{repeat:03d}"
            if isolated_root.exists() and any(isolated_root.iterdir()):
                raise RuntimeError(f"remote-cache isolated store is not empty: {isolated_root}")
            store = f"local?root={isolated_root}"
            local_probe = measure([nix, "path-info", "--store", store, output], cwd=repo)
            if local_probe.exit_code == 0:
                raise RuntimeError(f"remote-cache output is already valid locally: {output}")
            checked(
                measure([nix, "path-info", "--store", url, output], cwd=repo),
                "remote-cache advertisement probe",
            )
            verified_hits.append(
                {
                    "output": output,
                    "isolated_store": str(isolated_root),
                    "absent_before": True,
                    "advertised_by_cache": True,
                }
            )
            wrapper = root / f"remote-nix-{repeat:03d}"
            wrapper.write_text(
                f"#!/bin/sh\nexec {shlex.quote(nix)} --store {shlex.quote(store)} \"$@\"\n"
            )
            wrapper.chmod(0o755)
            remote_samples.append(
                checked(
                    measure(
                        [
                            str(engine),
                            "--nix",
                            str(wrapper),
                            "--substituter",
                            url,
                            "--trusted-public-key",
                            public_key,
                            "build",
                            "--flake",
                            flake,
                            package,
                            "--output",
                            "stream",
                        ],
                        cwd=repo,
                    ),
                    "remote-cache nix-tools build",
                )
            )
        operations["remote_cache_engine_build"] = {
            "samples": [sample.as_dict() for sample in remote_samples],
            "summary": measurement_summary(remote_samples),
            "verified_cache_hit": True,
            "preconditions": verified_hits,
        }
    if bun2nix is not None:
        lockfile = repo / "examples" / "bun-monorepo" / "bun.lock"
        operations["bun2nix_inspect"] = repeat_measure(
            [str(bun2nix), "inspect", "--lock-file", str(lockfile)],
            cwd=repo,
            repeats=repeats,
        )
        local_lockfile = (
            repo / "crates" / "bun2nix" / "tests" / "fixtures" / "corpus" / "local" / "bun.lock"
        )
        operations["bun2nix_parse_render_local"] = repeat_measure(
            [str(bun2nix), "convert", "--lock-file", str(local_lockfile)],
            cwd=local_lockfile.parent,
            repeats=repeats,
        )
        if bun2nix_external_lockfile is not None:
            operations["bun2nix_external_prefetch_convert"] = repeat_measure(
                [str(bun2nix), "convert", "--lock-file", str(bun2nix_external_lockfile)],
                cwd=bun2nix_external_lockfile.parent,
                repeats=repeats,
            )
    return {
        "operations": operations,
        "coverage": {
            "discovery_check_run": "covered per generated scenario by build/check/run engine paths",
            "bun2nix_prefetch": "opt in with --bun2nix-external-lockfile; omitted by default",
            "protocol_overhead": "not measurable: no cross-language engine protocol exists",
            "remote_cache": "opt in with paired URL/public-key flags; omitted by default",
        },
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
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--bun2nix", type=Path)
    parser.add_argument("--bun2nix-external-lockfile", type=Path)
    parser.add_argument("--remote-cache-url")
    parser.add_argument("--remote-cache-public-key")
    parser.add_argument("--remote-cache-flake")
    parser.add_argument("--remote-cache-package")
    parser.add_argument("--remote-cache-store-root", type=Path)
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    repo = args.repo.resolve()
    if args.repeats < 1:
        raise SystemExit("--repeats must be positive")
    try:
        remote_cache = validate_optional_group(
            (
                args.remote_cache_url,
                args.remote_cache_public_key,
                args.remote_cache_flake,
                args.remote_cache_package,
                args.remote_cache_store_root,
            ),
            "remote cache",
        )
    except ValueError as error:
        raise SystemExit(str(error)) from error
    if args.bun2nix_external_lockfile and not args.bun2nix:
        raise SystemExit("--bun2nix-external-lockfile requires --bun2nix")
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
            "schema_version": 2,
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
                "repeats": args.repeats,
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
            "auxiliary": benchmark_auxiliary_operations(
                repo=repo,
                engine=engine,
                nix=args.nix,
                repeats=args.repeats,
                bun2nix=args.bun2nix.resolve() if args.bun2nix else None,
                bun2nix_external_lockfile=args.bun2nix_external_lockfile.resolve()
                if args.bun2nix_external_lockfile
                else None,
                root=temporary_root,
                system=system,
                pinned_nixpkgs=pinned_nixpkgs,
                remote_cache=remote_cache,
            ),
        }
        output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
        for scenario in scenarios(args.targets):
            samples = [
                benchmark_scenario(
                    temporary_root / f"repeat-{repeat:03d}",
                    scenario,
                    nix=args.nix,
                    engine=engine,
                    fast_build=args.nix_fast_build,
                    system=system,
                    pinned_nixpkgs=pinned_nixpkgs,
                    trace_wrapper=trace_wrapper,
                    run_id=f"{args.run_id}-{repeat:03d}",
                )
                for repeat in range(args.repeats)
            ]
            results.append(summarize_scenario_samples(samples))
            output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    document["complete"] = True
    output.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n")
    print(output)


if __name__ == "__main__":
    main()
