package nixtools

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"
)

func TestEngineHelper(t *testing.T) {
	if os.Getenv("NIXTOOLS_TEST_EXEC") == "1" {
		if err := (PreparedRun{Program: "/bin/sh"}).Exec([]string{"-c", `printf '%s|%s' "$VALUE" "$PWD"; exit 19`}, []string{"VALUE=replaced"}, os.Getenv("NIXTOOLS_TEST_DIR")); err != nil {
			os.Exit(3)
		}
	}
	if os.Getenv("NIXTOOLS_TEST_ENGINE") != "1" {
		return
	}
	mode := os.Getenv("NIXTOOLS_TEST_MODE")
	if mode == "no_hello" {
		time.Sleep(time.Minute)
		os.Exit(0)
	}
	hello := os.Getenv("NIXTOOLS_TEST_HELLO")
	if hello == "" {
		hello = `{"type":"hello","version":1,"capabilities":["discover","build","check","build_installables","prepare_run","flake_check","rebuild","skip_cached","all_outputs","max_jobs","interactive_presentation"]}`
	}
	fmt.Println(hello)
	var request map[string]json.RawMessage
	control := os.Stdin
	if slices.Contains(os.Args, "--interactive") {
		control = os.NewFile(3, "control")
		defer control.Close()
	}
	decoder := json.NewDecoder(control)
	if err := decoder.Decode(&request); err != nil {
		os.Exit(3)
	}
	if mode == "presentation" {
		input, readErr := io.ReadAll(os.Stdin)
		if readErr != nil || string(input) != "terminal input" || string(request["presentation"]) != `{"mode":"tui","title":"Build example"}` || fmt.Sprint(syscall.Getpgrp()) != os.Getenv("NIXTOOLS_TEST_PGID") {
			os.Exit(8)
		}
		fmt.Fprintln(os.Stderr, "rendered on terminal")
	}
	if mode == "cancel" || mode == "cancel_manifest" {
		var cancel struct {
			Type   string
			Signal int
		}
		if decoder.Decode(&cancel) != nil || cancel.Type != "cancel" || cancel.Signal != 15 {
			os.Exit(4)
		}
		if mode == "cancel_manifest" {
			fmt.Println(`{"type":"result","version":1,"id":"1","result":{"kind":"build","manifest":{"outcome":"cancelled","roots":[{"name":"partial","state":"built"}]}}}`)
		}
		os.Exit(0)
	}
	if mode == "hang" {
		time.Sleep(time.Minute)
		os.Exit(0)
	}
	if mode == "stderr" {
		fmt.Fprint(os.Stderr, strings.Repeat("x", 100000))
	}
	if raw := os.Getenv("NIXTOOLS_TEST_FRAMES"); raw != "" {
		fmt.Print(raw)
	} else {
		var op string
		_ = json.Unmarshal(request["operation"], &op)
		if mode == "request" {
			if string(request["rebuild"]) != "true" || string(request["out_link"]) != `"result"` || string(request["targets"]) != `["selected"]` {
				os.Exit(5)
			}
		}
		if mode == "installables" {
			var config struct {
				Limits struct {
					MaxJobs uint64 `json:"max_jobs"`
				}
			}
			_ = json.Unmarshal(request["config"], &config)
			if op != "build_installables" || string(request["attribute_paths"]) != `[["legacyPackages","x86_64-linux","android app"]]` || config.Limits.MaxJobs != 2 || string(request["out_link"]) != `"result"` {
				os.Exit(10)
			}
		}
		if mode == "skipcached" && (string(request["skip_cached"]) != "true" || len(request["rebuild"]) != 0) {
			os.Exit(11)
		}
		if mode == "alloutputs" && (string(request["all_outputs"]) != "true" || len(request["rebuild"]) != 0) {
			os.Exit(12)
		}
		switch op {
		case "discover":
			fmt.Println(`{"type":"result","version":1,"id":"1","result":{"kind":"discover","discovery":{"packages":["hello"],"checks":[],"apps":[]}}}`)
		case "prepare_run":
			fmt.Println(`{"type":"result","version":1,"id":"1","result":{"kind":"prepare_run","program":"/bin/sh","manifest":{"outcome":"success"}}}`)
		case "flake_check":
			fmt.Println(`{"type":"result","version":1,"id":"1","result":{"kind":"flake_check","exit_code":0}}`)
		default:
			fmt.Printf("{\"type\":\"result\",\"version\":1,\"id\":\"1\",\"result\":{\"kind\":%q,\"manifest\":{\"outcome\":\"success\"}}}\n", op)
		}
	}
	if mode == "stderr" {
		os.Exit(7)
	}
	os.Exit(0)
}

func testClient(extra ...string) Client {
	return Client{Executable: os.Args[0], Arguments: []string{"-test.run=TestEngineHelper", "--"}, Environment: append(append(os.Environ(), "NIXTOOLS_TEST_ENGINE=1", "GORACE=atexit_sleep_ms=0"), extra...), Config: EngineConfig{System: "x86_64-linux"}, CancelGrace: 500 * time.Millisecond}
}

func TestDiscover(t *testing.T) {
	client := testClient()
	got, err := client.Discover(context.Background(), Flake{Reference: "."})
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Packages) != 1 || got.Packages[0] != "hello" {
		t.Fatalf("unexpected discovery: %+v", got)
	}
}

func TestOperations(t *testing.T) {
	c := testClient("NIXTOOLS_TEST_MODE=request")
	if _, err := c.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}, Targets: []string{"selected"}, OutLink: "result", Rebuild: true}); err != nil {
		t.Fatal(err)
	}
	if _, err := c.Check(context.Background(), CheckRequest{Flake: Flake{Reference: "."}, Targets: []string{"selected"}, OutLink: "result", Rebuild: true}); err != nil {
		t.Fatal(err)
	}
	c = testClient()
	if got, err := c.PrepareRun(context.Background(), RunRequest{Flake: Flake{Reference: "."}, App: "hello"}); err != nil || got.Program != "/bin/sh" {
		t.Fatalf("%+v %v", got, err)
	}
	if err := c.FlakeCheck(context.Background(), Flake{Reference: "."}); err != nil {
		t.Fatal(err)
	}
}

func TestProtocolFailures(t *testing.T) {
	tests := map[string]string{
		"bad_json": "not json\n", "truncated": "{", "unterminated": `{"type":"result","version":1,"id":"1","result":{"kind":"discover","discovery":{}}}`,
		"wrong_version":   "{\"type\":\"result\",\"version\":2,\"id\":\"1\"}\n",
		"wrong_id":        "{\"type\":\"result\",\"version\":1,\"id\":\"2\"}\n",
		"unknown":         "{\"type\":\"future\",\"version\":1,\"id\":\"1\"}\n",
		"missing_error":   "{\"type\":\"error\",\"version\":1,\"id\":\"1\"}\n",
		"missing_payload": "{\"type\":\"result\",\"version\":1,\"id\":\"1\",\"result\":{\"kind\":\"discover\"}}\n",
		"wrong_kind":      "{\"type\":\"result\",\"version\":1,\"id\":\"1\",\"result\":{\"kind\":\"build\"}}\n",
		"trailing":        "{\"type\":\"result\",\"version\":1,\"id\":\"1\",\"result\":{\"kind\":\"discover\",\"discovery\":{}}}\n{}\n",
	}
	for name, frames := range tests {
		t.Run(name, func(t *testing.T) {
			c := testClient("NIXTOOLS_TEST_FRAMES=" + frames)
			_, err := c.Discover(context.Background(), Flake{Reference: "."})
			var sdkErr *Error
			if !errors.As(err, &sdkErr) || sdkErr.Code != "protocol" {
				t.Fatalf("%v", err)
			}
		})
	}
}

func TestHelloCompatibility(t *testing.T) {
	for _, hello := range []string{`{"type":"hello","version":2}`, `{"type":"hello","version":1,"capabilities":[]}`, `{"type":"hello","version":1,"capabilities":["discover"],"max_request_bytes":2}`} {
		c := testClient("NIXTOOLS_TEST_HELLO=" + hello)
		if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
			t.Fatal("accepted incompatible hello")
		}
	}
	c := testClient(`NIXTOOLS_TEST_HELLO={"type":"hello","version":1,"capabilities":["build"]}`)
	if _, err := c.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}, Rebuild: true}); err == nil {
		t.Fatal("accepted missing rebuild capability")
	}
}

func TestEventsAndFailedManifests(t *testing.T) {
	frames := `{"type":"progress","version":1,"id":"1","event":{"kind":"future","data":{"extra":true}}}` + "\n" + `{"type":"result","version":1,"id":"1","result":{"kind":"build","manifest":{"outcome":"failed","roots":[{"name":"broken","state":"failed"}]}}}` + "\n"
	c := testClient("NIXTOOLS_TEST_FRAMES=" + frames)
	var event Event
	c.OnEvent = func(e Event) { event = e }
	manifest, err := c.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}})
	var sdkErr *Error
	if !errors.As(err, &sdkErr) || sdkErr.Manifest == nil || len(manifest.Roots) != 1 || event.Kind != "future" || string(event.Data) != `{"extra":true}` {
		t.Fatalf("%+v %v %+v", manifest, err, event)
	}
	frames = `{"type":"error","version":1,"id":"1","error":{"category":"nix","message":"failed","exit_code":17,"manifest":{"outcome":"failed"}}}` + "\n"
	c = testClient("NIXTOOLS_TEST_FRAMES=" + frames)
	manifest, err = c.Build(context.Background(), BuildRequest{Flake: Flake{Reference: "."}})
	if ExitCode(err) != 17 || manifest.Outcome != "failed" || !strings.Contains(err.Error(), "nix: failed") {
		t.Fatalf("%+v %v", manifest, err)
	}
}

func TestCancellation(t *testing.T) {
	for _, mode := range []string{"no_hello", "hang", "cancel", "cancel_manifest"} {
		t.Run(mode, func(t *testing.T) {
			c := testClient("NIXTOOLS_TEST_MODE=" + mode)
			ctx, cancel := context.WithCancelCause(context.Background())
			defer cancel(nil)
			timer := time.AfterFunc(50*time.Millisecond, func() { cancel(SignalCause{Signal: syscall.SIGTERM}) })
			defer timer.Stop()
			start := time.Now()
			manifest, err := c.Build(ctx, BuildRequest{Flake: Flake{Reference: "."}})
			if !errors.Is(err, context.Canceled) {
				t.Fatalf("%v", err)
			}
			if time.Since(start) > time.Second {
				t.Fatal("cancellation did not bound cleanup")
			}
			if mode == "cancel_manifest" && len(manifest.Roots) != 1 {
				t.Fatalf("lost partial manifest: %+v", manifest)
			}
			if mode == "cancel_manifest" && ExitCode(err) != 143 {
				t.Fatalf("signal exit code lost: %v", err)
			}
		})
	}
}

func TestValidationAndExitErrors(t *testing.T) {
	c := testClient()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := c.Discover(ctx, Flake{Reference: "."}); !errors.Is(err, context.Canceled) {
		t.Fatal(err)
	}
	for _, reference := range []string{"", "bad\x00ref", "bad\xffref"} {
		if _, err := c.Discover(context.Background(), Flake{Reference: reference}); err == nil {
			t.Fatal("accepted invalid reference")
		}
	}
	c.Config.System = ""
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("accepted missing system")
	}
	c = testClient()
	c.Executable = "/nonexistent/nix-tools"
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("missing engine accepted")
	}
	c = testClient("NIXTOOLS_TEST_MODE=stderr")
	c.Stderr = io.Discard
	_, err := c.Discover(context.Background(), Flake{Reference: "."})
	if err == nil || len(err.Error()) > 66000 {
		t.Fatalf("unexpected bounded diagnostic length: %v", err)
	}
	if ExitCode(nil) != 0 || ExitCode(context.Canceled) != 130 || ExitCode(context.DeadlineExceeded) != 124 || ExitCode(fmt.Errorf("other")) != 1 {
		t.Fatal("exit status mismatch")
	}
}

func TestAppExecution(t *testing.T) {
	p := PreparedRun{Program: "/bin/sh"}
	var output bytes.Buffer
	dir := t.TempDir()
	err := p.Execute(context.Background(), AppOptions{Arguments: []string{"-c", `IFS= read -r input; printf '%s|%s|%s' "$VALUE" "$PWD" "$input"`}, Environment: []string{"VALUE=app value"}, WorkingDirectory: dir, Stdin: strings.NewReader("input"), Stdout: &output, Stderr: &output})
	if err != nil || output.String() != "app value|"+dir+"|input" {
		t.Fatalf("%q %v", output.String(), err)
	}
	err = p.Execute(context.Background(), AppOptions{Arguments: []string{"-c", "exit 23"}})
	if ExitCode(err) != 23 {
		t.Fatal(err)
	}
	err = p.Execute(context.Background(), AppOptions{Arguments: []string{"-c", "kill -TERM $$"}, Stdin: os.Stdin})
	if ExitCode(err) != 143 {
		t.Fatal(err)
	}
	ctx, cancel := context.WithCancelCause(context.Background())
	timer := time.AfterFunc(20*time.Millisecond, func() { cancel(SignalCause{Signal: syscall.SIGTERM}) })
	defer timer.Stop()
	defer cancel(nil)
	err = p.Execute(ctx, AppOptions{Arguments: []string{"-c", "sleep 10"}})
	if ExitCode(err) != 143 {
		t.Fatal(err)
	}
	if err = (PreparedRun{Program: "/nonexistent"}).Execute(context.Background(), AppOptions{}); err == nil {
		t.Fatal("missing app accepted")
	}
}

func TestAppExec(t *testing.T) {
	dir := t.TempDir()
	cmd := exec.Command(os.Args[0], "-test.run=TestEngineHelper")
	cmd.Env = append(os.Environ(), "NIXTOOLS_TEST_EXEC=1", "NIXTOOLS_TEST_DIR="+dir)
	output, err := cmd.CombinedOutput()
	if ExitCode(err) != 19 || string(output) != "replaced|"+dir {
		t.Fatalf("%q %v", output, err)
	}
	before, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	if err = (PreparedRun{Program: "/nonexistent"}).Exec(nil, nil, dir); err == nil {
		t.Fatal("missing executable accepted")
	}
	after, err := os.Getwd()
	if err != nil || before != after {
		t.Fatalf("cwd not restored: %q %q %v", before, after, err)
	}
	if err = (PreparedRun{Program: "/nonexistent"}).Exec(nil, nil, ""); err == nil {
		t.Fatal("missing executable accepted")
	}
	if err = (PreparedRun{Program: "/bin/sh"}).Exec(nil, nil, "/nonexistent"); err == nil {
		t.Fatal("missing directory accepted")
	}
}

func TestResponseLimit(t *testing.T) {
	c := testClient("NIXTOOLS_TEST_FRAMES=" + strings.Repeat("x", 2048) + "\n")
	c.MaxResponseBytes = 1024
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil || !strings.Contains(err.Error(), "token too long") {
		t.Fatalf("%v", err)
	}
	c.MaxResponseBytes = 1
	if _, err := c.Discover(context.Background(), Flake{Reference: "."}); err == nil {
		t.Fatal("invalid response limit accepted")
	}
}

func TestGoldenCompatibility(t *testing.T) {
	input, err := os.ReadFile("testdata/discover-request.json")
	if err != nil {
		t.Fatal(err)
	}
	var r request
	if err = json.Unmarshal(input, &r); err != nil {
		t.Fatal(err)
	}
	if r.Type != "request" || r.Version != ProtocolVersion || r.ID != "1" || r.Operation != "discover" || r.Config.NixExecutable != "nix" {
		t.Fatalf("incompatible request %+v", r)
	}
	response, err := os.ReadFile("testdata/discover-result.json")
	if err != nil {
		t.Fatal(err)
	}
	c := testClient("NIXTOOLS_TEST_FRAMES=" + string(response))
	c.Config = r.Config
	got, err := c.Discover(context.Background(), r.Flake)
	if err != nil || len(got.Packages) != 1 || got.Packages[0] != "cli" || len(got.Checks) != 1 || got.Checks[0] != "lint" || len(got.Apps) != 1 || got.Apps[0] != "dev" {
		t.Fatalf("incompatible result %+v %v", got, err)
	}
}

func TestAppCancellationCleansDetachedStreams(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	ready := make(chan int, 1)
	writer := &pidWriter{ready: ready}
	result := make(chan error, 1)
	go func() {
		result <- (PreparedRun{Program: "/bin/sh"}).Execute(ctx, AppOptions{Arguments: []string{"-c", `sh -c 'trap "" INT TERM; while :; do sleep 1; done' </dev/null >/dev/null 2>&1 & echo $!; wait`}, Stdout: writer})
	}()
	var pid int
	select {
	case pid = <-ready:
	case <-time.After(3 * time.Second):
		t.Fatal("child did not start")
	}
	defer syscall.Kill(pid, syscall.SIGKILL)
	cancel()
	select {
	case <-result:
	case <-time.After(4 * time.Second):
		t.Fatal("application cancellation timed out")
	}
	// A killed orphan can remain a zombie until the host reaper runs.
	for deadline := time.Now().Add(time.Second); ; {
		if syscall.Kill(pid, 0) != nil {
			return
		}
		if stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid)); err == nil && strings.Contains(string(stat), ") Z ") {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("child %d survived application cancellation", pid)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

type pidWriter struct{ ready chan int }

func (w *pidWriter) Write(p []byte) (int, error) {
	var pid int
	if _, err := fmt.Sscanf(string(p), "%d", &pid); err == nil {
		select {
		case w.ready <- pid:
		default:
		}
	}
	return len(p), nil
}

func TestAppCooperativeCancellationStatus(t *testing.T) {
	ctx, cancel := context.WithCancelCause(context.Background())
	defer cancel(nil)
	ready := make(chan int, 1)
	result := make(chan error, 1)
	go func() {
		result <- (PreparedRun{Program: "/bin/sh"}).Execute(ctx, AppOptions{Arguments: []string{"-c", `trap 'exit 0' TERM; echo $$; while :; do :; done`}, Stdout: &pidWriter{ready: ready}})
	}()
	select {
	case <-ready:
	case <-time.After(3 * time.Second):
		t.Fatal("application did not start")
	}
	cancel(SignalCause{Signal: syscall.SIGTERM})
	select {
	case err := <-result:
		if ExitCode(err) != 143 {
			t.Fatalf("%v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("application did not stop")
	}
	if err := (PreparedRun{Program: "/bin/sh"}).Execute(ctx, AppOptions{}); ExitCode(err) != 143 {
		t.Fatalf("pre-cancelled: %v", err)
	}
}
