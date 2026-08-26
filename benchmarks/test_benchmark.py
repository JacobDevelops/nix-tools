import json
import unittest

from benchmark import (
    Scenario,
    classify_nix_command,
    invalidation_fan_out,
    graph_entries,
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


class ResultFormatTest(unittest.TestCase):
    def test_json_result_shape_is_stable(self) -> None:
        scenario = Scenario(targets=8, dependency_shape="shared")

        self.assertEqual(
            json.loads(json.dumps(scenario.as_dict(), sort_keys=True)),
            {"dependency_shape": "shared", "targets": 8},
        )


if __name__ == "__main__":
    unittest.main()
