package nixtools

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"strings"
	"testing"
)

func TestSharedFailureMessageRenderedOnce(t *testing.T) {
	expected := "flake check failed\nvalidation: invalid package output"
	failure := Error{Category: "process", Message: expected, Status: 19}
	payload, err := json.Marshal(failure)
	if err != nil {
		t.Fatal(err)
	}
	frames := `{"type":"result","version":1,"id":"1","failure":` + string(payload) + `,"result":{"kind":"flake_check","manifest":{"outcome":"failed","diagnostics":[{"phase":"validation","severity":"error","message":"validation: invalid package output"}]}}}` + "\n"
	output, err := os.CreateTemp(t.TempDir(), "terminal")
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	client := testClient("NIXTOOLS_TEST_FRAMES=" + frames)
	client.Presentation = &Presentation{Mode: "stream"}
	client.TerminalOutput = output
	manifest, err := client.FlakeCheck(context.Background(), Flake{Reference: "."})
	if err == nil || err.Error() != expected || ExitCode(err) != 19 || len(manifest.Diagnostics) != 1 {
		t.Fatalf("lost shared failure: %+v %v", manifest, err)
	}
	typed, ok := err.(*Error)
	if !ok || typed.Manifest == nil || typed.Category != "process" {
		t.Fatalf("lost structured failure: %v", err)
	}
	if _, writeErr := fmt.Fprintln(output, err); writeErr != nil {
		t.Fatal(writeErr)
	}
	rendered, readErr := os.ReadFile(output.Name())
	if readErr != nil || string(rendered) != expected+"\n" || strings.Count(string(rendered), "invalid package output") != 1 {
		t.Fatalf("failure rendered more than once: %q %v", rendered, readErr)
	}
}

func TestSharedCancellationRetainsCauseAndSignal(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	frames := `{"type":"progress","version":1,"id":"1","event":{"kind":"phase_started","data":"validation"}}` + "\n" + `{"type":"result","version":1,"id":"1","failure":{"category":"cancelled","message":"cancelled during validation","exit_code":143,"signal":15},"result":{"kind":"flake_check","manifest":{"outcome":"cancelled"}}}` + "\n"
	client := testClient("NIXTOOLS_TEST_FRAMES=" + frames)
	client.OnEvent = func(Event) { cancel() }
	manifest, err := client.FlakeCheck(ctx, Flake{Reference: "."})
	typed, ok := err.(*Error)
	if !ok || !errors.Is(err, context.Canceled) || ExitCode(err) != 143 || typed.Signal == nil || *typed.Signal != 15 || manifest.Outcome != "cancelled" {
		t.Fatalf("lost cancellation: %+v %v", manifest, err)
	}
}

func TestInvalidSharedFailureRejected(t *testing.T) {
	for _, outcome := range []string{"failed", "success"} {
		frames := `{"type":"result","version":1,"id":"1","failure":{"category":"process","message":"failed","exit_code":0},"result":{"kind":"build","manifest":{"outcome":"` + outcome + `"}}}` + "\n"
		client := testClient("NIXTOOLS_TEST_FRAMES=" + frames)
		_, err := client.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}})
		typed, ok := err.(*Error)
		if !ok || typed.Code != "protocol" {
			t.Fatalf("invalid shared failure accepted: %v", err)
		}
	}
}
