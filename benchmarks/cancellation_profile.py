"""Measure SIGTERM response after a command has stayed alive for 50 milliseconds."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def measure(command: list[str], repeats: int) -> list[dict]:
    samples = []
    for _ in range(repeats):
        process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                   start_new_session=True)
        try:
            time.sleep(0.05)
            if process.poll() is not None:
                raise ValueError("cancellation workload exited before SIGTERM")
            started = time.monotonic_ns()
            process.send_signal(signal.SIGTERM)
            status = process.wait(timeout=5)
            elapsed = time.monotonic_ns() - started
            try:
                os.killpg(process.pid, 0)
                descendants_remaining = True
            except ProcessLookupError:
                descendants_remaining = False
            samples.append({"latency_ns": elapsed, "returncode": status,
                            "descendants_remaining": descendants_remaining})
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
    return samples


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=20)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command or args.repeats < 1:
        parser.error("a command and at least one repeat are required")
    args.output.write_text(json.dumps({"command": command, "samples": measure(command, args.repeats)}, indent=2) + "\n")
