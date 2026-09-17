package nixtools

import (
	"context"
	"fmt"
	"os"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestPresentationTransport(t *testing.T) {
	input, err := os.CreateTemp(t.TempDir(), "terminal-input")
	if err != nil {
		t.Fatal(err)
	}
	defer input.Close()
	if _, err = input.WriteString("terminal input"); err != nil {
		t.Fatal(err)
	}
	if _, err = input.Seek(0, 0); err != nil {
		t.Fatal(err)
	}
	output, err := os.CreateTemp(t.TempDir(), "terminal-output")
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	c := testClient("NIXTOOLS_TEST_MODE=presentation", fmt.Sprintf("NIXTOOLS_TEST_PGID=%d", syscall.Getpgrp()))
	c.Presentation = &Presentation{Mode: "tui", Title: "Build example"}
	c.TerminalInput = input
	c.TerminalOutput = output
	got, err := c.Discover(context.Background(), Flake{Reference: "."})
	if err != nil || len(got.Packages) != 1 {
		t.Fatalf("%+v %v", got, err)
	}
	rendered, err := os.ReadFile(output.Name())
	if err != nil || string(rendered) != "rendered on terminal\n" {
		t.Fatalf("%q %v", rendered, err)
	}
}

func TestPresentationCancellationAndCapability(t *testing.T) {
	output, err := os.OpenFile(os.DevNull, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	for _, mode := range []string{"cancel", "hang"} {
		t.Run(mode, func(t *testing.T) {
			c := testClient("NIXTOOLS_TEST_MODE=" + mode)
			c.Presentation = &Presentation{Mode: "stream"}
			c.TerminalOutput = output
			ctx, cancel := context.WithCancelCause(context.Background())
			defer cancel(nil)
			timer := time.AfterFunc(50*time.Millisecond, func() { cancel(SignalCause{Signal: syscall.SIGTERM}) })
			defer timer.Stop()
			if _, err = c.Build(ctx, BuildRequest{Flake: Flake{Reference: "."}}); ExitCode(err) != 143 {
				t.Fatal(err)
			}
		})
	}
	c := testClient(`NIXTOOLS_TEST_HELLO={"type":"hello","version":1,"capabilities":["discover"]}`)
	c.Presentation = &Presentation{Mode: "tui"}
	c.TerminalOutput = output
	if _, err = c.Discover(context.Background(), Flake{Reference: "."}); err == nil || !strings.Contains(err.Error(), "presentation") {
		t.Fatal(err)
	}
}

func TestPresentationValidationAndOutputFailure(t *testing.T) {
	c := testClient()
	c.Presentation = &Presentation{Mode: "invalid"}
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("invalid presentation accepted")
	}
	c.Presentation = &Presentation{Mode: "tui", Title: "bad\x00title"}
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("invalid presentation title accepted")
	}
	c.Presentation = &Presentation{Mode: "stream"}
	c.Stderr = &strings.Builder{}
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("non-terminal output silently ignored")
	}
	output, err := os.CreateTemp(t.TempDir(), "closed-output")
	if err != nil {
		t.Fatal(err)
	}
	output.Close()
	c.Stderr = nil
	c.TerminalOutput = output
	if _, err = c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("closed terminal output accepted")
	}
}

func TestFullFlakeCheckRetainsFailedManifest(t *testing.T) {
	c := testClient("NIXTOOLS_TEST_FRAMES=" + `{"type":"result","version":1,"id":"1","result":{"kind":"flake_check","exit_code":1,"manifest":{"outcome":"failed","roots":[{"name":"test","state":"failed"}]}}}` + "\n")
	_, err := c.FlakeCheck(context.Background(), Flake{Reference: "."})
	typed, ok := err.(*Error)
	if !ok || typed.Manifest == nil || len(typed.Manifest.Roots) != 1 || typed.Manifest.Roots[0].Name != "test" {
		t.Fatalf("lost failed full-check manifest: %v", err)
	}
}
