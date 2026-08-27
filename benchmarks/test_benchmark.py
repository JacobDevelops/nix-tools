import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import benchmark
from benchmark import (
    Measurement,
    Scenario,
    _linux_pid_and_parent,
    benchmark_engine,
    benchmark_scenario,
    classify_nix_command,
    graph_entries,
    invalidation_fan_out,
    output_paths,
    percentile,
    repeat_measure,
    summarize_scenario_samples,
    validate_optional_group,
    render_flake,
    scenarios,
)


class ScenarioTest(unittest.TestCase):
    def test_matrix_covers_required_sizes_and_dependency_shapes(self) -> None:
        matrix = scenarios()

        self.assertEqual(len(matrix), 8)
        self.assertEqual({scenario.targets for scenario in matrix}, {1, 8, 32, 128})
        self.assertEqual({scenario.dependency_shape for scenario in matrix}, {"shared", "exclusive"})

    def test_rejects_unknown_dependency_shape(self) -> None:
        with self.assertRaisesRegex(ValueError, "dependency shape"):
            Scenario(targets=8, dependency_shape="mixed")


class FixtureTest(unittest.TestCase):
    def test_shared_fixture_has_one_dependency_consumed_by_every_target(self) -> None:
        fixture = render_flake(
            Scenario(targets=8, dependency_shape="shared"),
            system="x86_64-linux",
            nixpkgs_url="github:NixOS/nixpkgs/abc123",
            mutation=0,
        )

        self.assertEqual(fixture.count('runCommand "benchmark-shared-dependency"'), 1)
        self.assertEqual(fixture.count("cat ${sharedDependency}"), 8)
        self.assertEqual(fixture.count("exclusiveDependency"), 0)

    def test_exclusive_fixture_has_one_dependency_per_target(self) -> None:
        fixture = render_flake(
            Scenario(targets=8, dependency_shape="exclusive"),
            system="aarch64-darwin",
            nixpkgs_url="github:NixOS/nixpkgs/abc123",
            mutation=0,
        )

        self.assertEqual(fixture.count('runCommand "benchmark-exclusive-'), 8)
        self.assertEqual(fixture.count("cat ${exclusiveDependency"), 8)
        self.assertNotIn("sharedDependency", fixture)

    def test_mutation_changes_shared_dependency_but_only_first_exclusive_dependency(self) -> None:
        shared = render_flake(
            Scenario(targets=8, dependency_shape="shared"),
            system="x86_64-linux",
            nixpkgs_url="github:NixOS/nixpkgs/abc123",
            mutation=1,
        )
        exclusive = render_flake(
            Scenario(targets=8, dependency_shape="exclusive"),
            system="x86_64-linux",
            nixpkgs_url="github:NixOS/nixpkgs/abc123",
            mutation=1,
        )

        self.assertEqual(shared.count("mutation-1"), 1)
        self.assertEqual(exclusive.count("mutation-1"), 1)
        self.assertEqual(exclusive.count("stable-exclusive"), 7)


class MetricsTest(unittest.TestCase):
    def test_percentile_uses_linear_interpolation(self) -> None:
        self.assertEqual(percentile([1.0, 2.0, 3.0, 4.0], 0.5), 2.5)
        self.assertEqual(percentile([1.0, 2.0, 3.0, 4.0], 0.95), 3.85)

    def test_repeat_measure_preserves_samples_and_summarizes_distributions(self) -> None:
        samples = [
            Measurement(value, index, index * 10, index * 100, 0, b"", b"")
            for index, value in enumerate((1.0, 2.0, 10.0), start=1)
        ]
        with patch.object(benchmark, "measure", side_effect=samples):
            result = repeat_measure(["tool", "--version"], cwd=Path("/tmp"), repeats=3)

        self.assertEqual(len(result["samples"]), 3)
        self.assertEqual(result["summary"]["wall_seconds"]["p50"], 2.0)
        self.assertEqual(result["summary"]["wall_seconds"]["p95"], 9.2)
        self.assertEqual(result["summary"]["process_count"]["max"], 3)

    def test_repeat_measure_rejects_non_positive_repeats(self) -> None:
        with self.assertRaisesRegex(ValueError, "repeats"):
            repeat_measure(["true"], cwd=Path("/tmp"), repeats=0)

    def test_repeat_measure_rejects_nonzero_expected_success_sample(self) -> None:
        failed = Measurement(0.1, 1, None, 12, 2, b"", b"failed")
        with (
            patch.object(benchmark, "measure", return_value=failed),
            self.assertRaisesRegex(RuntimeError, "benchmark command tool failed"),
        ):
            repeat_measure(["tool"], cwd=Path("/tmp"), repeats=1)

    def test_scenario_summary_preserves_samples_and_percentiles_numeric_leaves(self) -> None:
        samples = [
            {
                "scenario": {"targets": 1, "dependency_shape": "shared"},
                "invalidation_fan_out": 1,
                "implementations": {
                    "nix-tools": {"phases": {"realization": {"wall_seconds": wall}}}
                },
            }
            for wall in (1.0, 2.0, 10.0)
        ]

        result = summarize_scenario_samples(samples)

        wall = result["summary"]["implementations"]["nix-tools"]["phases"][
            "realization"
        ]["wall_seconds"]
        self.assertEqual(result["sample_count"], 3)
        self.assertEqual(wall, {"min": 1.0, "p50": 2.0, "p95": 9.2, "max": 10.0})

    def test_parses_parent_pid_after_comm_with_spaces_and_parentheses(self) -> None:
        stat = "123 (worker ) with spaces) S 42 1 1 0 -1"

        self.assertEqual(_linux_pid_and_parent(stat), (123, 42))

    def test_classifies_engine_nix_subcommands(self) -> None:
        self.assertEqual(classify_nix_command(["nix", "eval", "--json"]), "evaluation")
        self.assertEqual(
            classify_nix_command(["nix", "derivation", "show", "--recursive"]),
            "graph_construction",
        )
        self.assertEqual(
            classify_nix_command(["nix", "path-info", "--json", "--stdin"]),
            "cache_probe",
        )
        self.assertEqual(classify_nix_command(["nix", "build", "--stdin"]), "realization")
        self.assertEqual(classify_nix_command(["nix", "store", "ping"]), "other")

    def test_counts_only_changed_target_derivations(self) -> None:
        baseline = {"target-000": "/nix/store/a.drv", "target-001": "/nix/store/b.drv"}
        mutated = {"target-000": "/nix/store/c.drv", "target-001": "/nix/store/b.drv"}

        self.assertEqual(invalidation_fan_out(baseline, mutated), 1)

    def test_rejects_incomparable_invalidation_sets(self) -> None:
        with self.assertRaisesRegex(ValueError, "target sets"):
            invalidation_fan_out({"target-000": "a"}, {"target-001": "b"})

    def test_reads_versioned_nix_derivation_json(self) -> None:
        graph = {
            "version": 3,
            "derivations": {
                "a.drv": {"outputs": {"out": {"path": "a"}}},
                "b.drv": {"outputs": {"out": {"path": "b"}}},
            },
        }

        self.assertEqual(len(graph_entries(graph)), 2)
        self.assertEqual(output_paths(graph), ["/nix/store/a", "/nix/store/b"])

    def test_engine_metrics_use_unwrapped_nix_while_attribution_uses_trace(self) -> None:
        measurements = [
            Measurement(value, 1, 1, 1, 0, b"", b"")
            for value in (10.0, 11.0, 20.0, 21.0, 22.0, 23.0)
        ]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root / "timed"
            trace_fixture = root / "traced"
            fixture.mkdir()
            trace_fixture.mkdir()
            wrapper = root / "trace-nix"
            with (
                patch.object(benchmark, "derivation_graph", return_value={"drv": {}}),
                patch.object(benchmark, "output_paths", return_value=["/nix/store/out"]),
                patch.object(benchmark, "valid_path_count", side_effect=[0, 1]),
                patch.object(
                    benchmark,
                    "aggregate_trace",
                    side_effect=[
                        {"realization": {"invocations": 1}},
                        {"cache_probe": {"invocations": 1}},
                    ],
                ),
                patch.object(benchmark, "measure", side_effect=measurements) as measure,
                patch.object(benchmark, "graph_entries", return_value={"drv": {}}),
            ):
                result = benchmark_engine(
                    "nix",
                    Path("/bin/nix-tools"),
                    fixture,
                    trace_fixture,
                    Scenario(1, "shared"),
                    "x86_64-linux",
                    wrapper,
                )

        commands = [call.args[0] for call in measure.call_args_list]
        self.assertEqual(commands[0][2], str(wrapper))
        self.assertEqual(commands[1][2], str(wrapper))
        self.assertEqual(commands[2][2], "nix")
        self.assertEqual(commands[3][2], "nix")
        self.assertEqual(measure.call_args_list[0].kwargs["cwd"], trace_fixture)
        self.assertEqual(measure.call_args_list[2].kwargs["cwd"], fixture)
        self.assertEqual(result["phases"]["realization"]["wall_seconds"], 20.0)
        self.assertEqual(result["phases"]["no_op_rebuild"]["wall_seconds"], 21.0)
        self.assertEqual(
            result["phases"]["selected_check_discovery"]["wall_seconds"], 22.0
        )
        self.assertIn("target:000", commands[4])
        self.assertEqual(result["phases"]["run"]["wall_seconds"], 23.0)

    def test_engine_trace_fixture_uses_a_distinct_salt(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            with (
                patch.object(benchmark, "write_fixture") as write_fixture,
                patch.object(benchmark, "benchmark_engine", return_value={}),
                patch.object(benchmark, "benchmark_plain", return_value={}),
                patch.object(benchmark, "target_derivations", return_value={"target": "drv"}),
            ):
                benchmark_scenario(
                    Path(temporary),
                    Scenario(1, "shared"),
                    nix="nix",
                    engine=Path("/bin/nix-tools"),
                    fast_build=None,
                    system="x86_64-linux",
                    pinned_nixpkgs="github:NixOS/nixpkgs/abc123",
                    trace_wrapper=Path(temporary) / "trace-nix",
                    run_id="run",
                )

        salts = [call.kwargs["salt"] for call in write_fixture.call_args_list]
        self.assertIn("nix-tools-run", salts)
        self.assertIn("nix-tools-trace-run", salts)


class ResultFormatTest(unittest.TestCase):
    def test_auxiliary_uses_real_engine_cancellation_and_independent_remote_fixtures(self) -> None:
        success = Measurement(0.1, 1, None, 0, 0, b"", b"")
        cancelled = Measurement(2.0, 1, None, 0, 143, b"", b"")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with (
                patch.object(benchmark, "repeat_measure", return_value={}),
                patch.object(
                    benchmark,
                    "measure",
                    side_effect=lambda command, **_: Measurement(
                        0.1,
                        1,
                        None,
                        0,
                        1 if "path-info" in command and "local?root=" in " ".join(command) else 0,
                        b"",
                        b"",
                    ),
                ) as measure,
                patch.object(benchmark, "measure_cancellation", return_value=cancelled) as cancel,
                patch.object(benchmark, "expected_package_output", return_value="/nix/store/out"),
            ):
                benchmark.benchmark_auxiliary_operations(
                    repo=root,
                    engine=Path("/bin/nix-tools"),
                    nix="nix",
                    repeats=2,
                    bun2nix=None,
                    bun2nix_external_lockfile=None,
                    root=root,
                    system="x86_64-linux",
                    pinned_nixpkgs="github:NixOS/nixpkgs/abc",
                    remote_cache=(
                        "https://cache.example",
                        "cache.example:key",
                        "github:owner/repo",
                        "package",
                        root / "stores",
                    ),
                )

        self.assertTrue(all(call.args[0][0] == "/bin/nix-tools" for call in cancel.call_args_list))
        remote_commands = [
            call.args[0]
            for call in measure.call_args_list
            if call.args[0] and call.args[0][0] == "/bin/nix-tools"
        ]
        self.assertEqual(len(remote_commands), 2)
        self.assertTrue(all("--substituter" in command for command in remote_commands))

    def test_remote_cache_hit_arguments_must_all_be_supplied(self) -> None:
        with self.assertRaisesRegex(ValueError, "together"):
            validate_optional_group(
                ("https://cache.example", "key", None, None, None), "remote cache"
            )

    def test_json_result_shape_is_stable(self) -> None:
        scenario = Scenario(targets=8, dependency_shape="shared")

        self.assertEqual(
            json.loads(json.dumps(scenario.as_dict(), sort_keys=True)),
            {"dependency_shape": "shared", "targets": 8},
        )

    def test_cli_exposes_repeat_count(self) -> None:
        with patch("sys.argv", ["benchmark.py", "--repeats", "7"]):
            self.assertEqual(benchmark.parse_args().repeats, 7)

    def test_cli_accepts_opt_in_remote_cache_and_external_bun_lockfile(self) -> None:
        with patch(
            "sys.argv",
            [
                "benchmark.py",
                "--remote-cache-url",
                "https://cache.example",
                "--remote-cache-public-key",
                "cache.example:key",
                "--remote-cache-flake",
                "github:owner/repo",
                "--remote-cache-package",
                "package",
                "--remote-cache-store-root",
                "/tmp/isolated-stores",
                "--bun2nix-external-lockfile",
                "/tmp/external-bun.lock",
            ],
        ):
            args = benchmark.parse_args()

        self.assertEqual(args.remote_cache_url, "https://cache.example")
        self.assertEqual(args.remote_cache_public_key, "cache.example:key")
        self.assertEqual(args.remote_cache_flake, "github:owner/repo")
        self.assertEqual(args.remote_cache_package, "package")
        self.assertEqual(args.bun2nix_external_lockfile, Path("/tmp/external-bun.lock"))


if __name__ == "__main__":
    unittest.main()
