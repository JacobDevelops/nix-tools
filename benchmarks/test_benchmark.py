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
            for value in (10.0, 11.0, 20.0, 21.0)
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
    def test_json_result_shape_is_stable(self) -> None:
        scenario = Scenario(targets=8, dependency_shape="shared")

        self.assertEqual(
            json.loads(json.dumps(scenario.as_dict(), sort_keys=True)),
            {"dependency_shape": "shared", "targets": 8},
        )


if __name__ == "__main__":
    unittest.main()
