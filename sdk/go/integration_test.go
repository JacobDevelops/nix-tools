//go:build integration

package nixtools_test

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	nixtools "github.com/JacobDevelops/nix-tools/sdk/go"
)

func TestRealEngineLifecycle(t *testing.T) {
	client, flake := realEngineFixture(t, false)
	discovery, err := client.Discover(t.Context(), flake)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(discovery.Packages, []string{"bad", "good", "slow"}) || !slices.Equal(discovery.Checks, []string{"good"}) || !slices.Equal(discovery.Apps, []string{"shell"}) {
		t.Fatalf("unexpected discovery: %+v", discovery)
	}
	outLink := filepath.Join(flake.WorkingDirectory, "selected-result")
	request := nixtools.BuildRequest{Flake: flake, Targets: []string{"good"}, OutLink: outLink}
	manifest, err := client.Build(t.Context(), request)
	if err != nil || manifest.Outcome != "success" || len(manifest.Roots) != 1 {
		t.Fatalf("build: %+v, %v", manifest, err)
	}
	content, err := os.ReadFile(outLink)
	if err != nil || string(content) != "built\n" {
		t.Fatalf("output link: %q, %v", content, err)
	}
	warm, err := client.Build(t.Context(), request)
	if err != nil || warm.Roots[0].State != "cached" {
		t.Fatalf("warm build: %+v, %v", warm, err)
	}
	request.Rebuild = true
	rebuilt, err := client.Build(t.Context(), request)
	if err != nil || rebuilt.Roots[0].State == "cached" || rebuilt.Metrics.Realization.Processes == 0 {
		t.Fatalf("rebuild did not execute: %+v, %v", rebuilt, err)
	}
	checked, err := client.Check(t.Context(), nixtools.CheckRequest{Flake: flake, Targets: []string{"good"}, Rebuild: true})
	if err != nil || checked.Outcome != "success" {
		t.Fatalf("check: %+v, %v", checked, err)
	}
	if err := client.FlakeCheck(t.Context(), flake); err != nil {
		t.Fatalf("valid flake rejected: %v", err)
	}
	prepared, err := client.PrepareRun(t.Context(), nixtools.RunRequest{Flake: flake, App: "shell"})
	if err != nil {
		t.Fatal(err)
	}
	var output bytes.Buffer
	err = prepared.Execute(t.Context(), nixtools.AppOptions{
		Arguments:        []string{"-c", `printf '%s\n%s\n%s\n' "$SDK_APP_ONLY" "$PWD" "$1"`, "sdk-test", "two words"},
		Environment:      []string{"SDK_APP_ONLY=private-value"},
		WorkingDirectory: flake.WorkingDirectory,
		Stdout:           &output,
		Stderr:           &output,
	})
	if err != nil || output.String() != "private-value\n"+flake.WorkingDirectory+"\ntwo words\n" {
		t.Fatalf("prepared app environment/cwd/arguments: %q, %v", output.String(), err)
	}
}

func TestRealEngineRetainsFailedManifest(t *testing.T) {
	client, flake := realEngineFixture(t, false)
	manifest, err := client.Build(t.Context(), nixtools.BuildRequest{Flake: flake, Targets: []string{"bad", "good"}})
	if err == nil || manifest.Outcome != "failed" || len(manifest.Roots) != 2 {
		t.Fatalf("failure lost manifest: %+v, %v", manifest, err)
	}
	if manifest.Roots[0].State != "failed" || manifest.Roots[1].State == "failed" || manifest.Roots[1].State == "skipped" {
		t.Fatalf("independent root was lost: %+v", manifest.Roots)
	}
}

func TestRealEngineFullFlakeValidation(t *testing.T) {
	client, flake := realEngineFixture(t, true)
	if _, err := client.Check(t.Context(), nixtools.CheckRequest{Flake: flake, Targets: []string{"good"}}); err != nil {
		t.Fatalf("selected check failed: %v", err)
	}
	if err := client.FlakeCheck(t.Context(), flake); err == nil {
		t.Fatal("full flake validation accepted an invalid app")
	}
}

func TestRealEngineCancellation(t *testing.T) {
	client, flake := realEngineFixture(t, false)
	ctx, cancel := context.WithTimeout(t.Context(), 10*time.Second)
	defer cancel()
	started := false
	client.OnEvent = func(event nixtools.Event) {
		if event.Kind == "node_started" {
			started = true
			cancel()
		}
	}
	manifest, err := client.Build(ctx, nixtools.BuildRequest{Flake: flake, Targets: []string{"slow"}})
	if !started || err == nil || manifest.Outcome != "cancelled" || nixtools.ExitCode(err) != 130 {
		t.Fatalf("cancellation lost active build outcome: started=%v, manifest=%+v, err=%v", started, manifest, err)
	}
}

func TestRealEngineCIInstallables(t *testing.T) {
	client, flake := realEngineFixture(t, false)
	jobs := uint64(2)
	client.Config.Limits.MaxJobs = &jobs
	request := nixtools.BuildInstallablesRequest{
		Flake:          flake,
		AttributePaths: [][]string{{"legacyPackages", client.Config.System, "hidden.with.dot"}},
	}
	manifest, err := client.BuildInstallables(t.Context(), request)
	if err != nil || manifest.Outcome != "success" || len(manifest.Roots) != 1 {
		t.Fatalf("explicit CI installable: %+v, %v", manifest, err)
	}
	request.SkipCached = true
	warm, err := client.BuildInstallables(t.Context(), request)
	if err != nil || warm.Outcome != "success" || warm.Roots[0].State != "cached" {
		t.Fatalf("skip-cached local installable: %+v, %v", warm, err)
	}
	request.SkipCached = false
	again, err := client.BuildInstallables(t.Context(), request)
	if err != nil || again.Roots[0].State != "cached" || again.Metrics.Realization.Processes != 0 {
		t.Fatalf("disabling skip-cached must not force rebuild: %+v, %v", again, err)
	}
	request.AttributePaths[0][2] = "multiple.outputs"
	selected, err := client.BuildInstallables(t.Context(), request)
	if err != nil || len(selected.Roots[0].Outputs) != 1 {
		t.Fatalf("default output selection: %+v, %v", selected, err)
	}
	request.AllOutputs = true
	all, err := client.BuildInstallables(t.Context(), request)
	if err != nil || len(all.Roots[0].Outputs) != 2 {
		t.Fatalf("CI all-output selection: %+v, %v", all, err)
	}
}

func realEngineFixture(t *testing.T, invalidApp bool) (nixtools.Client, nixtools.Flake) {
	t.Helper()
	engine := os.Getenv("NIX_TOOLS_ENGINE")
	if !filepath.IsAbs(engine) {
		t.Fatal("integration tests require NIX_TOOLS_ENGINE pointing at the built engine")
	}
	bash, err := exec.LookPath("bash")
	if err != nil {
		t.Fatal(err)
	}
	bash, err = filepath.EvalSymlinks(bash)
	if err != nil || !strings.HasPrefix(bash, "/nix/store/") {
		t.Fatalf("integration tests require Nix-provided bash: %s, %v", bash, err)
	}
	arch := map[string]string{"amd64": "x86_64", "arm64": "aarch64"}[runtime.GOARCH]
	system := arch + "-" + runtime.GOOS
	directory := t.TempDir()
	appType := "app"
	if invalidApp {
		appType = "invalid"
	}
	flake := fmt.Sprintf(`{
  inputs = {};
  outputs = { self }: let
    system = %s;
	    bash = builtins.appendContext %s { %s = { path = true; }; };
    make = name: script: let drv = builtins.derivation {
      name = name + %s;
      inherit system;
      builder = bash;
      args = [ "-c" script ];
    }; in drv // { outputs = [ "out" ]; out = drv; meta.outputsToInstall = [ "out" ]; };
    good = make "sdk-good" "echo built > $out";
    bad = make "sdk-bad" "echo deliberate-failure >&2; exit 1";
    slow = make "sdk-slow" "while true; do :; done";
    multi = (builtins.derivation {
      name = "sdk-multiple-outputs";
      inherit system;
      builder = bash;
      outputs = [ "out" "dev" ];
      args = [ "-c" "echo main > $out; echo development > $dev" ];
    }) // { meta.outputsToInstall = [ "out" ]; };
  in {
    packages.${system} = { inherit good bad slow; };
    checks.${system} = { inherit good; };
    legacyPackages.${system} = { "hidden.with.dot" = good; "multiple.outputs" = multi; };
    apps.${system}.shell = {
      type = %s;
      program = builtins.appendContext bash { ${builtins.unsafeDiscardStringContext good.drvPath} = { outputs = [ "out" ]; }; };
    };
  };
}`, strconv.Quote(system), strconv.Quote(bash), strconv.Quote(filepath.Dir(filepath.Dir(bash))), strconv.Quote(filepath.Base(filepath.Dir(directory))), strconv.Quote(appType))
	if err := os.WriteFile(filepath.Join(directory, "flake.nix"), []byte(flake), 0o600); err != nil {
		t.Fatal(err)
	}
	return nixtools.Client{Executable: engine, Config: nixtools.EngineConfig{System: system}}, nixtools.Flake{Reference: ".", WorkingDirectory: directory}
}
