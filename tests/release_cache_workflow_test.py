import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/publish-cache.yml"
CACHE_CONFIG = ROOT / "nix/cache/default.nix"
FLAKE = ROOT / "flake.nix"
DOCUMENTATION = ROOT / "docs/binary-cache.md"


class ReleaseCacheWorkflowTest(unittest.TestCase):
    def test_builds_every_supported_native_system(self) -> None:
        workflow = WORKFLOW.read_text()

        for runner, system in [
            ("ubuntu-24.04", "x86_64-linux"),
            ("ubuntu-24.04-arm", "aarch64-linux"),
            ("macos-15-intel", "x86_64-darwin"),
            ("macos-15", "aarch64-darwin"),
        ]:
            self.assertIn(f"runner: {runner}", workflow)
            self.assertIn(f"system: {system}", workflow)

    def test_only_the_serialized_publish_job_receives_write_secrets(self) -> None:
        workflow = WORKFLOW.read_text()
        build, publish = workflow.split("\n  publish:", 1)

        self.assertNotIn("R2_ACCESS_KEY_ID", build)
        self.assertNotIn("R2_SECRET_ACCESS_KEY", build)
        self.assertNotIn("NIX_CACHE_PRIVATE_KEY", build)
        self.assertIn("R2_ACCESS_KEY_ID", publish)
        self.assertIn("R2_SECRET_ACCESS_KEY", publish)
        self.assertIn("NIX_CACHE_PRIVATE_KEY", publish)
        self.assertIn("unset NIX_CACHE_PRIVATE_KEY", publish)
        self.assertIn("cancel-in-progress: false", workflow)

    def test_publish_credentials_are_gated_to_main_environment(self) -> None:
        workflow = WORKFLOW.read_text()
        _, publish = workflow.split("\n  publish:", 1)

        self.assertIn("if: github.ref == 'refs/heads/main'", publish)
        self.assertIn("environment: cache-publishing", publish)
        self.assertIn("restricted to the `main` branch", DOCUMENTATION.read_text())

    def test_actions_are_commit_pinned_and_prs_cannot_publish(self) -> None:
        workflow = WORKFLOW.read_text()

        self.assertNotIn("pull_request:", workflow)
        self.assertRegex(workflow, r"permissions:\n  contents: read")
        for reference in re.findall(r"uses: [^@\s]+@([^\s]+)", workflow):
            self.assertRegex(reference, r"^[0-9a-f]{40}$")

    def test_every_shell_step_stops_on_failure(self) -> None:
        workflow = WORKFLOW.read_text()

        self.assertEqual(workflow.count("set -euo pipefail"), workflow.count("run: |"))

    def test_flake_pins_the_public_cache_identity(self) -> None:
        config = CACHE_CONFIG.read_text()
        flake = FLAKE.read_text()
        documentation = DOCUMENTATION.read_text()

        for value in [
            "https://nix-tools-cache.jacobdevelops.com",
            "nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM=",
        ]:
            self.assertIn(value, config)
            self.assertIn(value, flake)
            self.assertIn(value, documentation)

    def test_publishes_a_signed_complete_runtime_closure(self) -> None:
        workflow = WORKFLOW.read_text()

        self.assertNotIn("--no-recursive", workflow)
        self.assertIn("compression=zstd", workflow)
        self.assertIn("secret-key=$key_file", workflow)

    def test_signing_key_matches_the_pinned_identity_before_upload(self) -> None:
        workflow = WORKFLOW.read_text()
        key_check = 'nix key convert-secret-to-public < "$key_file"'
        identity_check = 'test "$derived_public_key" = "$PUBLIC_KEY"'
        first_upload = 'nix copy --from "file://$cache" --to "$destination"'

        self.assertLess(workflow.index(key_check), workflow.index(identity_check))
        self.assertLess(workflow.index(identity_check), workflow.index(first_upload))

    def test_verifies_every_root_through_the_public_cache(self) -> None:
        workflow = WORKFLOW.read_text()

        self.assertIn('nix copy --from "$PUBLIC_CACHE"', workflow)
        self.assertIn('nix store verify --store "$verification_store"', workflow)
        self.assertIn("--sigs-needed 1", workflow)
        self.assertIn('--trusted-public-keys "$PUBLIC_KEY"', workflow)


if __name__ == "__main__":
    unittest.main()
