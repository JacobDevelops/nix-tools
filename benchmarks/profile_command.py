"""Profile a repeatable command; allocator/syscall runs never contaminate timing."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import time


def allocation_count(report: str) -> int:
    match = re.search(r"calls to allocation functions: (\d+)", report)
    if not match:
        raise ValueError("heaptrack allocation summary missing")
    return int(match[1])


def allocated_bytes(histogram: str) -> int:
    rows = [line.split() for line in histogram.splitlines() if line.strip()]
    if not rows or any(len(row) != 2 for row in rows):
        raise ValueError("heaptrack allocation histogram missing or malformed")
    return sum(int(size) * int(count) for size, count in rows)


def peak_heap_bytes(report: str) -> int:
    match = re.search(r"peak heap memory consumption: ([0-9.]+)([BKMG])", report)
    if not match:
        raise ValueError("heaptrack peak heap summary missing")
    return round(float(match[1]) * {"B": 1, "K": 1000, "M": 1000000, "G": 1000000000}[match[2]])


def syscall_count(report: str) -> int:
    for line in report.splitlines():
        fields = line.split()
        if fields and fields[-1] == "total" and len(fields) in (5, 6):
            return int(fields[3])
    raise ValueError("strace total missing")


def run(command: list[str], path: Path) -> None:
    with path.open("wb") as log:
        subprocess.run(command, stdout=subprocess.DEVNULL, stderr=log, check=True)


def timed(command: list[str], log_path: Path) -> dict:
    with log_path.open("wb") as log:
        started = time.monotonic_ns()
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=log)
        _, status, usage = os.wait4(process.pid, 0)
        process.returncode = os.waitstatus_to_exitcode(status)
        wall = (time.monotonic_ns() - started) / 1e9
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, command)
    return {"user_seconds": usage.ru_utime, "system_seconds": usage.ru_stime,
            "cpu_seconds": usage.ru_utime + usage.ru_stime, "wall_seconds": wall,
            "peak_rss_kib": usage.ru_maxrss}


def profile(command: list[str], directory: Path, repeats: int) -> dict:
    directory.mkdir(parents=True, exist_ok=True)
    samples = []
    for index in range(repeats):
        prefix = directory / str(index)
        timing = timed(command, prefix.with_suffix(".stdout"))
        trace = prefix.with_suffix(".strace")
        run(["strace", "-f", "-c", "-o", str(trace), *command], prefix.with_suffix(".strace.stdout"))
        heap = prefix.with_suffix(".heap")
        run(["heaptrack", "-o", str(heap), *command], prefix.with_suffix(".heap.stdout"))
        files = list(directory.glob(heap.name + ".*"))
        profiles = [path for path in files if path.suffix in (".gz", ".zst")]
        if len(profiles) != 1:
            raise ValueError(f"expected one heaptrack profile: {profiles}")
        histogram = prefix.with_suffix(".heap.histogram")
        report = subprocess.check_output(["heaptrack_print", "-H", str(histogram), str(profiles[0])], text=True)
        prefix.with_suffix(".heap.txt").write_text(report)
        samples.append({**timing,
                        "allocation_calls": allocation_count(report), "peak_heap_bytes": peak_heap_bytes(report),
                        "allocated_bytes": allocated_bytes(histogram.read_text()), "syscalls": syscall_count(trace.read_text())})
    return {"command": command, "cwd": os.getcwd(), "samples": samples}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or args.repeats < 1:
        parser.error("a command and at least one repeat are required")
    report = profile(command, args.output.with_suffix(""), args.repeats)
    args.output.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
