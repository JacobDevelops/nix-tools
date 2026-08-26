import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/publish-cache.yml"
RELEASE_WORKFLOW = ROOT / ".github/workflows/release.yml"
CACHE_CONFIG = ROOT / "nix/cache/default.nix"
FLAKE = ROOT / "flake.nix"
DOCUMENTATION = ROOT / "docs/binary-cache.md"
BUN_DOCUMENTATION = ROOT / "docs/bun.md"
PUBLIC_FLAKES = [
    FLAKE,
    ROOT / "examples/framework/flake.nix",
    ROOT / "examples/bun-monorepo/flake.nix",
]


class ReleaseCacheWorkflowTest(unittest.TestCase):
    def test_builds_every_supported_native_system(self) -> None:
        workflow = WORKFLOW.read_text()

        for runner, system in [
            ("ubuntu-24.04", "x86_64-linux"),
            ("ubuntu-24.04-arm", "aarch64-linux"),
            ("macos-15", "aarch64-darwin"),
        ]:
            self.assertIn(f'"runner":"{runner}"', workflow)
            self.assertIn(f'"system":"{system}"', workflow)

        self.assertNotIn("macos-15-intel", workflow)
        self.assertNotIn("x86_64-darwin", workflow)

    def test_public_flakes_only_expose_supported_nixpkgs_systems(self) -> None:
        for flake in PUBLIC_FLAKES:
            self.assertNotIn('"x86_64-darwin"', flake.read_text())

        root_flake = FLAKE.read_text()
        self.assertIn("github:NixOS/nixpkgs/nixpkgs-26.05-darwin", root_flake)

    def test_only_builds_missing_signed_output_hashes(self) -> None:
        workflow = WORKFLOW.read_text()
        plan, build = workflow.split("\n  build:", 1)

        self.assertIn("\n  plan:", plan)
        self.assertIn('output=$(nix eval --raw ".#packages.$system.bun2nix.outPath")', plan)
        self.assertIn('nix copy --from "$PUBLIC_CACHE"', plan)
        self.assertIn("--option require-sigs true", plan)
        self.assertIn("--retry-all-errors", plan)
        self.assertIn('if test "$status" = 404', plan)
        self.assertIn('test "$status" = 200', plan)
        self.assertIn("publish-needed: ${{ steps.plan.outputs.publish-needed }}", plan)
        self.assertIn("matrix: ${{ fromJSON(needs.plan.outputs.matrix) }}", build)
        self.assertIn("EXPECTED_OUTPUT: ${{ matrix.output }}", build)
        self.assertIn('test "$output" = "$EXPECTED_OUTPUT"', build)
        self.assertIn("needs.plan.outputs.publish-needed == 'true'", workflow)
        self.assertIn("Nix store path", DOCUMENTATION.read_text())

    def test_empty_plan_allocates_only_an_inert_linux_sentinel(self) -> None:
        workflow = WORKFLOW.read_text()
        _, build = workflow.split("\n  build:", 1)
        build, _ = build.split("\n  publish:", 1)

        self.assertIn('"runner":"ubuntu-24.04"', workflow)
        self.assertIn('"skip":true', workflow)
        self.assertIn("publish_needed=false", workflow)
        self.assertEqual(build.count("if: matrix.skip != true"), 4)

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

        self.assertIn("github.ref == 'refs/heads/main'", publish)
        self.assertIn("needs.plan.outputs.publish-needed == 'true'", publish)
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
        bun_documentation = BUN_DOCUMENTATION.read_text()

        for value in [
            "https://nix-tools-cache.jacobdevelops.com",
            "nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM=",
        ]:
            self.assertIn(value, config)
            self.assertIn(value, flake)
            self.assertIn(value, documentation)
            self.assertIn(value, bun_documentation)

    def test_bun_guide_documents_prebuilt_cli_usage(self) -> None:
        documentation = BUN_DOCUMENTATION.read_text()

        self.assertIn("nix run --accept-flake-config github:JacobDevelops/nix-tools/v0.1.0#bun2nix", documentation)
        self.assertIn("nix profile add --accept-flake-config github:JacobDevelops/nix-tools/v0.1.0#bun2nix", documentation)
        self.assertIn('nix-tools.url = "github:JacobDevelops/nix-tools/v0.1.0";', documentation)
        self.assertIn("nix-tools.packages.${system}.bun2nix", documentation)
        self.assertIn("Do not make `nix-tools/nixpkgs` follow", documentation)

    def test_release_requires_matching_version_and_signed_native_closures(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text()
        verify_job, release_job = workflow.split("\n  release:", 1)
        verify = "nix copy --from \"$PUBLIC_CACHE\""
        release = 'gh release create "$tag"'

        self.assertIn("workflow_run:", workflow)
        self.assertIn("group: release-version", workflow)
        self.assertIn("workflows: [CI]", workflow)
        self.assertIn("branches: [main]", workflow)
        self.assertIn("github.event.workflow_run.event == 'push'", workflow)
        self.assertIn("github.event.workflow_run.conclusion == 'success'", workflow)
        self.assertIn("ref: ${{ github.event.workflow_run.head_sha }}", workflow)
        self.assertRegex(workflow, r"permissions:\n  contents: read")
        self.assertIn("permissions:\n      contents: write", workflow)
        self.assertNotIn("contents: write", verify_job)
        self.assertNotIn("uses:", release_job)
        self.assertIn("v$version", workflow)
        self.assertIn(".#packages.$system.bun2nix.outPath", workflow)
        self.assertIn(verify, workflow)
        self.assertIn("--option require-sigs true", workflow)
        self.assertIn("needs: verify", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("GH_REPO: ${{ github.repository }}", release_job)
        self.assertIn("gh release view", workflow)
        self.assertIn('gh api "repos/$GH_REPO/commits/$tag"', workflow)
        self.assertIn('test "$tagged_sha" = "$release_sha"', workflow)
        self.assertIn('tag_status=$(curl', workflow)
        self.assertIn('case "$tag_status" in', workflow)
        self.assertIn("404)", workflow)
        self.assertIn("--verify-tag", workflow)
        self.assertIn("--target \"$release_sha\"", workflow)
        self.assertIn("--generate-notes", workflow)
        self.assertLess(workflow.index(verify), workflow.index(release))
        self.assertLess(workflow.index("404)"), workflow.index('--target "$release_sha"'))
        self.assertNotIn("NIX_CACHE_PRIVATE_KEY", workflow)
        self.assertNotIn("R2_SECRET_ACCESS_KEY", workflow)
        self.assertIn("successful `main` CI", DOCUMENTATION.read_text())
        for reference in re.findall(r"uses: [^@\s]+@([^\s]+)", workflow):
            self.assertRegex(reference, r"^[0-9a-f]{40}$")

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
