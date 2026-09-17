package nixtools

import (
	"context"
	"testing"
)

func TestBuildInstallablesPreservesAttributeComponentsAndBudget(t *testing.T) {
	c := testClient("NIXTOOLS_TEST_MODE=installables")
	jobs := uint64(2)
	c.Config.Limits.MaxJobs = &jobs
	got, err := c.BuildInstallables(context.Background(), BuildInstallablesRequest{Flake: Flake{Reference: "."}, AttributePaths: [][]string{{"legacyPackages", "x86_64-linux", "android app"}}, OutLink: "result"})
	if err != nil || got.Outcome != "success" {
		t.Fatalf("%+v %v", got, err)
	}
}

func TestSkipCachedIsSeparateFromRebuild(t *testing.T) {
	c := testClient("NIXTOOLS_TEST_MODE=skipcached")
	flake := Flake{Reference: "."}
	if _, err := c.Build(context.Background(), BuildRequest{Flake: flake, SkipCached: true}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Check(context.Background(), CheckRequest{Flake: flake, SkipCached: true}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.BuildInstallables(context.Background(), BuildInstallablesRequest{Flake: flake, AttributePaths: [][]string{{"packages", "x86_64-linux", "hello"}}, SkipCached: true}); err != nil {
		t.Fatal(err)
	}
}

func TestExtendedCapabilityValidation(t *testing.T) {
	c := testClient(`NIXTOOLS_TEST_HELLO={"type":"hello","version":1,"capabilities":["build"]}`)
	if _, err := c.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}, SkipCached: true}); err == nil {
		t.Fatal("missing skip_cached capability accepted")
	}
	jobs := uint64(2)
	c.Config.Limits.MaxJobs = &jobs
	if _, err := c.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}}); err == nil {
		t.Fatal("missing max_jobs capability accepted")
	}
}

func TestAllOutputsPreservesNormalRebuildPolicy(t *testing.T) {
	c := testClient("NIXTOOLS_TEST_MODE=alloutputs")
	flake := Flake{Reference: "."}
	if _, err := c.Build(context.Background(), BuildRequest{Flake: flake, AllOutputs: true}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Check(context.Background(), CheckRequest{Flake: flake, AllOutputs: true}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.BuildInstallables(context.Background(), BuildInstallablesRequest{Flake: flake, AttributePaths: [][]string{{"packages", "x86_64-linux", "hello"}}, AllOutputs: true}); err != nil {
		t.Fatal(err)
	}
	c = testClient(`NIXTOOLS_TEST_HELLO={"type":"hello","version":1,"capabilities":["build"]}`)
	if _, err := c.Build(context.Background(), BuildRequest{Flake: flake, AllOutputs: true}); err == nil {
		t.Fatal("missing all_outputs capability accepted")
	}
}
