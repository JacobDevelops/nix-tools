import unittest
import sys

from profile_command import allocation_count, syscall_count


class ParsersTest(unittest.TestCase):
    def test_allocation_summary(self):
        self.assertEqual(allocation_count('calls to allocation functions: 1234 (99/s)'), 1234)

    def test_syscall_summary_with_errors(self):
        self.assertEqual(syscall_count('100.00 0.004 9 432 12 total'), 432)

    def test_syscall_summary_without_errors(self):
        self.assertEqual(syscall_count('100.00 0.004 9 432 total'), 432)

    def test_missing_metric_is_error(self):
        for parser in (allocation_count, syscall_count):
            with self.assertRaises(ValueError):
                parser('unavailable')


class ProfileTest(unittest.TestCase):
    def test_profile_keeps_independent_measurements(self):
        import tempfile
        from pathlib import Path
        from unittest.mock import patch
        from profile_command import profile

        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)

            def execute(command, log):
                if command[0] == "strace":
                    Path(command[4]).write_text("100.00 0.01 1 30 2 total")
                else:
                    Path(command[2] + ".zst").touch()
                    (directory / "0.heap.histogram").write_text("8 4\n16 2")

            with patch("profile_command.timed", return_value={"cpu_seconds": 0.375, "wall_seconds": 0.5}), patch("profile_command.run", side_effect=execute), patch(
                "profile_command.subprocess.check_output",
                return_value="calls to allocation functions: 42 (99/s)\npeak heap memory consumption: 12.50K",
            ):
                sample = profile(["true"], directory, 1)["samples"][0]
            self.assertEqual(sample["cpu_seconds"], 0.375)
            self.assertEqual(sample["wall_seconds"], 0.5)
            self.assertEqual(sample["allocation_calls"], 42)
            self.assertEqual(sample["peak_heap_bytes"], 12500)
            self.assertEqual(sample["allocated_bytes"], 64)
            self.assertEqual(sample["syscalls"], 30)

    def test_failed_command_is_not_reported_as_success(self):
        import subprocess
        import tempfile
        from pathlib import Path
        from profile_command import run

        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(subprocess.CalledProcessError):
                run([sys.executable, "-c", "raise SystemExit(7)"], Path(tmp) / "log")


class CancellationTest(unittest.TestCase):
    def test_live_process_receives_sigterm(self):
        from cancellation_profile import measure
        sample = measure([sys.executable, "-c", "import time; time.sleep(10)"], 1)[0]
        self.assertEqual(sample["returncode"], -15)
        self.assertGreater(sample["latency_ns"], 0)

    def test_early_exit_is_not_a_cancellation_sample(self):
        from cancellation_profile import measure
        with self.assertRaisesRegex(ValueError, "exited before SIGTERM"):
            measure([sys.executable, "-c", "pass"], 1)


class FixturesTest(unittest.TestCase):
    def test_workloads_include_shared_graph_and_duplicate_sources(self):
        import json
        import tempfile
        from pathlib import Path
        from optimization_fixtures import generate

        with tempfile.TemporaryDirectory() as tmp:
            directory = Path(tmp)
            generate(directory)
            closures = json.loads((directory / "bun-closures.lock").read_text())
            sources = json.loads((directory / "bun-prefetch.lock").read_text())
            self.assertEqual(len(closures["workspaces"]), 201)
            self.assertEqual(len(closures["packages"]), 150)
            self.assertEqual(len(sources["packages"]), 64)
            self.assertEqual(len({row[0].split("@", 1)[1] for row in sources["packages"].values()}), 8)
            self.assertTrue((directory / "fake-bin/nix").stat().st_mode & 0o100)


class TimingTest(unittest.TestCase):
    def test_kernel_usage_has_subcentisecond_precision(self):
        import tempfile
        from pathlib import Path
        from profile_command import timed
        with tempfile.TemporaryDirectory() as tmp:
            sample = timed([sys.executable, "-c", "pass"], Path(tmp) / "log")
        self.assertGreater(sample["cpu_seconds"], 0)
        self.assertGreater(sample["wall_seconds"], 0)

    def test_nonzero_status_rejected(self):
        import subprocess
        import tempfile
        from pathlib import Path
        from profile_command import timed
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(subprocess.CalledProcessError):
                timed([sys.executable, "-c", "raise SystemExit(1)"], Path(tmp) / "log")


class CancellationCliTest(unittest.TestCase):
    def test_cli_writes_cancellation_evidence(self):
        import json
        import runpy
        import tempfile
        from pathlib import Path
        from unittest.mock import patch
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp) / "report.json"
            with patch("sys.argv", ["cancellation_profile", "--output", str(output), "--repeats", "1", "--", sys.executable, "-c", "import time; time.sleep(10)"]):
                runpy.run_module("cancellation_profile", run_name="__main__")
            self.assertEqual(len(json.loads(output.read_text())["samples"]), 1)

    def test_remaining_child_group_is_reported_and_cleaned(self):
        from cancellation_profile import measure
        sample = measure([sys.executable, "-c", "import subprocess,sys,time; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(10)']); time.sleep(10)"], 1)[0]
        self.assertTrue(sample["descendants_remaining"])
