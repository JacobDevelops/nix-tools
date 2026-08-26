import re
import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github/workflows/ci.yml"
CACHE_CONFIG = ROOT / "nix/cache/default.nix"
FLAKE = ROOT / "flake.nix"
DOCUMENTATION = ROOT / "docs/binary-cache.md"
BUN_DOCUMENTATION = ROOT / "docs/bun.md"
BUN2NIX_MANIFEST = ROOT / "crates/bun2nix/Cargo.toml"
CI_SCRIPTS = ROOT / ".github/workflows/scripts"
PLAN_RELEASE_CACHE = CI_SCRIPTS / "plan-release-cache.sh"
BUILD_RELEASE_CACHE = CI_SCRIPTS / "build-release-cache.sh"
PUBLISH_CACHE = CI_SCRIPTS / "publish-cache.sh"
VERIFY_CACHE = CI_SCRIPTS / "verify-cache.sh"
BUILD_CI_CACHE = CI_SCRIPTS / "build-ci-cache.sh"
VERIFY_RELEASE = CI_SCRIPTS / "verify-release.sh"
CREATE_RELEASE = CI_SCRIPTS / "create-release.sh"
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
        plan_script = PLAN_RELEASE_CACHE.read_text()
        build_script = BUILD_RELEASE_CACHE.read_text()

        self.assertIn("\n  plan:", plan)
        self.assertIn(".github/workflows/scripts/plan-release-cache.sh", plan)
        self.assertIn('output=$(nix eval --raw ".#packages.$system.bun2nix.outPath")', plan_script)
        self.assertIn('nix copy --from "$PUBLIC_CACHE"', plan_script)
        self.assertIn("--option require-sigs true", plan_script)
        self.assertIn("--retry-all-errors", plan_script)
        self.assertIn('if test "$status" = 404', plan_script)
        self.assertIn('test "$status" = 200', plan_script)
        self.assertIn("publish-needed: ${{ steps.plan.outputs.publish-needed }}", plan)
        self.assertIn("matrix: ${{ fromJSON(needs.plan.outputs.matrix) }}", build)
        self.assertIn("EXPECTED_OUTPUT: ${{ matrix.output }}", build)
        self.assertIn('test "$output" = "$EXPECTED_OUTPUT"', build_script)
        self.assertIn("needs.plan.outputs.publish-needed == 'true'", workflow)
        self.assertIn("Nix store path", DOCUMENTATION.read_text())

    def test_empty_plan_allocates_only_an_inert_linux_sentinel(self) -> None:
        workflow = WORKFLOW.read_text()
        _, build = workflow.split("\n  build:", 1)
        build, _ = build.split("\n  publish:", 1)

        self.assertIn('"runner":"ubuntu-24.04"', workflow)
        self.assertIn('"skip":true', PLAN_RELEASE_CACHE.read_text())
        self.assertIn("publish_needed=false", PLAN_RELEASE_CACHE.read_text())
        self.assertEqual(build.count("if: matrix.skip != true"), 4)

    def test_only_the_serialized_publish_job_receives_write_secrets(self) -> None:
        workflow = WORKFLOW.read_text()
        _, release_jobs = workflow.split("\n  plan:", 1)
        build, publish = release_jobs.split("\n  publish:", 1)

        self.assertNotIn("R2_ACCESS_KEY_ID", build)
        self.assertNotIn("R2_SECRET_ACCESS_KEY", build)
        self.assertNotIn("NIX_CACHE_PRIVATE_KEY", build)
        self.assertIn("R2_ACCESS_KEY_ID", publish)
        self.assertIn("R2_SECRET_ACCESS_KEY", publish)
        self.assertIn("NIX_CACHE_PRIVATE_KEY", publish)
        self.assertIn("unset NIX_CACHE_PRIVATE_KEY", PUBLISH_CACHE.read_text())
        self.assertIn("cancel-in-progress: ${{ github.event_name == 'pull_request' }}", workflow)

    def test_publish_credentials_are_gated_to_main_environment(self) -> None:
        workflow = WORKFLOW.read_text()
        _, publish = workflow.split("\n  publish:", 1)

        self.assertIn("needs: publish-ci", workflow)
        self.assertIn("github.event_name == 'push'", workflow)
        self.assertIn("github.ref == 'refs/heads/main'", workflow)
        self.assertIn("needs.plan.outputs.publish-needed == 'true'", publish)
        self.assertIn("environment: cache-publishing", publish)
        self.assertIn("restricted to the `main` branch", DOCUMENTATION.read_text())

    def test_actions_are_commit_pinned_and_prs_cannot_publish(self) -> None:
        workflow = WORKFLOW.read_text()

        self.assertIn("pull_request:", workflow)
        check, _ = workflow.split("\n  publish-ci:", 1)
        self.assertNotIn("NIX_CACHE_PRIVATE_KEY", check)
        self.assertRegex(workflow, r"permissions:\n  contents: read")
        for reference in re.findall(r"uses: [^@\s]+@([^\s]+)", workflow):
            self.assertRegex(reference, r"^[0-9a-f]{40}$")

    def test_every_shell_step_stops_on_failure(self) -> None:
        workflow = WORKFLOW.read_text()

        for script in CI_SCRIPTS.glob("*.sh"):
            self.assertIn("set -euo pipefail", script.read_text())

    def test_flake_pins_the_public_cache_identity(self) -> None:
        config = CACHE_CONFIG.read_text()
        flake = FLAKE.read_text()
        documentation = DOCUMENTATION.read_text()
        bun_documentation = BUN_DOCUMENTATION.read_text()

        for value in [
            "https://releases.nix-tools.jacobdevelops.com",
            "https://cache.nix-tools.jacobdevelops.com",
            "nix-tools-cache-1:L//AlyivgCsAry2QZdCyryq9nrQxi6x0usW4Pwfp7cM=",
        ]:
            self.assertIn(value, config)
            self.assertIn(value, flake)
            self.assertIn(value, documentation)
            self.assertIn(value, bun_documentation)

    def test_bun_guide_documents_prebuilt_cli_usage(self) -> None:
        documentation = BUN_DOCUMENTATION.read_text()

        self.assertIn("nix run --accept-flake-config github:JacobDevelops/nix-tools#bun2nix", documentation)
        self.assertIn("nix profile add --accept-flake-config github:JacobDevelops/nix-tools#bun2nix", documentation)
        self.assertIn('nix-tools.url = "github:JacobDevelops/nix-tools";', documentation)
        self.assertIn("nix-tools.packages.${system}.bun2nix", documentation)
        self.assertIn("Do not make `nix-tools/nixpkgs` follow", documentation)

    def test_release_requires_matching_version_and_signed_native_closures(self) -> None:
        workflow = WORKFLOW.read_text()
        verify_job, release_job = workflow.split("\n  release:", 1)
        verify_script = VERIFY_RELEASE.read_text()
        release_script = CREATE_RELEASE.read_text()

        self.assertIn("builtins.readFile ./crates/bun2nix/Cargo.toml", verify_script)
        self.assertIn("group: ci-", workflow)
        self.assertIn("push:\n    branches:\n      - main", workflow)
        self.assertIn("RELEASE_SHA: ${{ github.sha }}", workflow)
        self.assertRegex(workflow, r"permissions:\n  contents: read")
        self.assertIn("permissions:\n      contents: write", workflow)
        self.assertNotIn("contents: write", verify_job)
        self.assertIn("persist-credentials: false", release_job)
        self.assertIn("v$version", verify_script)
        self.assertIn('tag="bun2nix-v$version"', verify_script)
        self.assertIn(".#packages.$system.bun2nix.outPath", verify_script)
        self.assertIn('nix copy --from "$PUBLIC_CACHE"', verify_script)
        self.assertIn("--option require-sigs true", verify_script)
        self.assertIn("Authorization: Bearer $GH_TOKEN", verify_script)
        self.assertIn('case "$release_status" in', verify_script)
        self.assertIn("needs: verify", workflow)
        self.assertIn("persist-credentials: false", workflow)
        self.assertIn("GH_REPO: ${{ github.repository }}", release_job)
        self.assertIn("gh release view", release_script)
        self.assertIn('gh api "repos/$GH_REPO/commits/$TAG"', release_script)
        self.assertIn('test "$tagged_sha" = "$RELEASE_SHA"', release_script)
        self.assertIn('tag_status=$(curl', release_script)
        self.assertIn('case "$tag_status" in', release_script)
        self.assertIn("404)", release_script)
        self.assertIn("--verify-tag", release_script)
        self.assertIn("--target \"$RELEASE_SHA\"", release_script)
        self.assertIn("--generate-notes", release_script)
        self.assertIn("github:JacobDevelops/nix-tools/$TAG#bun2nix", release_script)
        self.assertIn("https://releases.nix-tools.jacobdevelops.com", workflow)
        self.assertLess(workflow.index(".github/workflows/scripts/verify-release.sh"), workflow.index(".github/workflows/scripts/create-release.sh"))
        self.assertLess(release_script.index("404)"), release_script.index('--target "$RELEASE_SHA"'))
        self.assertLess(workflow.index(".github/workflows/scripts/publish-release-cache.sh"), workflow.index(".github/workflows/scripts/verify-release.sh"))
        self.assertIn("Successful `main` CI", DOCUMENTATION.read_text())
        self.assertIn("bun2nix package version", DOCUMENTATION.read_text())
        self.assertIn("Earlier releases remain immutable", DOCUMENTATION.read_text())
        for reference in re.findall(r"uses: [^@\s]+@([^\s]+)", workflow):
            self.assertRegex(reference, r"^[0-9a-f]{40}$")

    def test_release_cache_skips_versions_that_are_already_released(self) -> None:
        workflow = WORKFLOW.read_text()
        script = PLAN_RELEASE_CACHE.read_text()

        self.assertIn('tag="bun2nix-v$version"', script)
        self.assertIn("Authorization: Bearer $GH_TOKEN", script)
        self.assertIn('case "$release_status" in', script)
        self.assertIn("404) ;;", script)
        self.assertIn("release lookup failed", script)
        self.assertIn("nix-tools-releases", workflow)
        _, release_publish = workflow.split("\n  publish:", 1)
        self.assertNotIn('R2_BUCKET: nix-tools-cache\n', release_publish)

    def test_ci_reads_both_caches_without_magic_nix_cache(self) -> None:
        workflow = WORKFLOW.read_text()

        self.assertNotIn("magic-nix-cache-action", workflow)
        self.assertIn("https://releases.nix-tools.jacobdevelops.com", workflow)
        self.assertIn("https://cache.nix-tools.jacobdevelops.com", workflow)
        self.assertIn("extra-trusted-public-keys", workflow)

    def test_ci_publishes_its_existing_outputs_only_on_main(self) -> None:
        workflow = WORKFLOW.read_text()
        check, publish = workflow.split("\n  publish-ci:", 1)

        self.assertIn(".github/workflows/scripts/build-ci-cache.sh", check)
        self.assertNotIn("NIX_CACHE_PRIVATE_KEY", check)
        self.assertIn("needs: check", publish)
        self.assertIn("github.event_name == 'push'", publish)
        self.assertIn("github.ref == 'refs/heads/main'", publish)
        self.assertIn("environment: cache-publishing", publish)
        self.assertIn("NIX_CACHE_PRIVATE_KEY", publish)
        self.assertIn("R2_BUCKET: nix-tools-cache", publish)
        self.assertIn("nix eval --json '.#checks.x86_64-linux'", BUILD_CI_CACHE.read_text())
        self.assertIn("./examples/bun-monorepo#checks.x86_64-linux", BUILD_CI_CACHE.read_text())
        self.assertIn("nix copy", PUBLISH_CACHE.read_text())
        for reference in re.findall(r"uses: [^@\s]+@([^\s]+)", workflow):
            self.assertRegex(reference, r"^[0-9a-f]{40}$")

    def test_bun2nix_has_an_independent_release_version(self) -> None:
        manifest = BUN2NIX_MANIFEST.read_text()

        self.assertIn('version = "0.2.2"', manifest)
        self.assertNotIn("\nversion.workspace = true\n", manifest)

    def test_publishes_a_signed_complete_runtime_closure(self) -> None:
        workflow = WORKFLOW.read_text() + PUBLISH_CACHE.read_text()

        self.assertNotIn("--no-recursive", workflow)
        self.assertIn("compression=zstd", workflow)
        self.assertIn("secret-key=$key_file", workflow)

    def test_signing_key_matches_the_pinned_identity_before_upload(self) -> None:
        workflow = PUBLISH_CACHE.read_text()
        key_check = 'nix key convert-secret-to-public < "$key_file"'
        identity_check = 'test "$derived_public_key" = "$PUBLIC_KEY"'
        first_upload = 'nix copy --from "file://$CACHE_DIRECTORY" --to "$destination"'

        self.assertLess(workflow.index(key_check), workflow.index(identity_check))
        self.assertLess(workflow.index(identity_check), workflow.index(first_upload))

    def test_verifies_every_root_through_the_public_cache(self) -> None:
        workflow = VERIFY_CACHE.read_text()

        self.assertIn('nix copy --from "$PUBLIC_CACHE"', workflow)
        self.assertIn('nix store verify --store "$verification_store"', workflow)
        self.assertIn("--sigs-needed 1", workflow)
        self.assertIn('--trusted-public-keys "$PUBLIC_KEY"', workflow)


if __name__ == "__main__":
    unittest.main()
