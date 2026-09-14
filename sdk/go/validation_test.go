package nixtools

import (
	"context"
	"encoding/json"
	"strings"
	"testing"
)

func TestFullFlakeCheckValidationManifestAndEvents(t *testing.T) {
	for _, outcome := range []string{"success", "failed", "cancelled"} {
		t.Run(outcome, func(t *testing.T) {
			frames := `{"type":"progress","version":1,"id":"1","event":{"kind":"phase_started","data":"validation"}}` + "\n" + `{"type":"progress","version":1,"id":"1","event":{"kind":"phase_finished","data":"validation"}}` + "\n"
			manifest := Manifest{Outcome: outcome, Metrics: ManifestMetrics{Validation: PhaseMetrics{Processes: 1, DurationMS: 7}}, Diagnostics: []Diagnostic{{Phase: "validation", Code: "validation_output", Severity: "info", Stderr: "evaluating flake outputs"}}}
			if outcome != "success" {
				manifest.Diagnostics = append(manifest.Diagnostics, Diagnostic{Phase: "validation", Code: "nix_failure", Severity: "error", Message: "invalid flake output", Stderr: "validation detail"})
			}
			payload, err := json.Marshal(manifest)
			if err != nil {
				t.Fatal(err)
			}
			exitCode := "0"
			if outcome == "failed" {
				exitCode = "1"
			}
			if outcome == "cancelled" {
				exitCode = "143"
			}
			frames += `{"type":"result","version":1,"id":"1","signal":15,"result":{"kind":"flake_check","exit_code":` + exitCode + `,"manifest":` + string(payload) + "}}\n"
			client := testClient("NIXTOOLS_TEST_FRAMES=" + frames)
			var events []Event
			client.OnEvent = func(e Event) { events = append(events, e) }
			got, err := client.FlakeCheck(context.Background(), Flake{Reference: "."})
			if got.Metrics.Validation.Processes != 1 || got.Metrics.Validation.DurationMS != 7 || got.Diagnostics[0].Severity != "info" || got.Diagnostics[0].Stderr != "evaluating flake outputs" {
				t.Fatalf("lost validation evidence: %+v", got)
			}
			if len(events) != 2 || events[0].Kind != "phase_started" || events[1].Kind != "phase_finished" || string(events[0].Data) != `"validation"` || string(events[1].Data) != `"validation"` {
				t.Fatalf("unexpected validation events: %+v", events)
			}
			if outcome == "success" {
				if err != nil {
					t.Fatal(err)
				}
				return
			}
			typed, ok := err.(*Error)
			if !ok || typed.Manifest == nil || len(typed.Manifest.Diagnostics) != 2 || typed.Manifest.Diagnostics[1].Stderr != "validation detail" {
				t.Fatalf("lost failure manifest: %v", err)
			}
			if strings.Contains(err.Error(), "validation detail") {
				t.Fatalf("error duplicates renderer diagnostics: %v", err)
			}
			if outcome == "cancelled" && ExitCode(err) != 143 {
				t.Fatalf("lost cancellation signal: %v", err)
			}
		})
	}
}
