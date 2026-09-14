package nixtools

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"reflect"
	"slices"
	"strings"
	"syscall"
	"time"
	"unicode/utf8"
)

// A Client starts an isolated engine for each operation; callbacks run synchronously.
type Client struct {
	Executable       string
	Arguments        []string
	Environment      []string
	Config           EngineConfig
	OnEvent          func(Event)
	Stderr           io.Writer
	CancelGrace      time.Duration
	MaxResponseBytes int
	Presentation     *Presentation
	TerminalInput    *os.File
	TerminalOutput   *os.File
}
type request struct {
	Type             string        `json:"type"`
	Version          int           `json:"version"`
	ID               string        `json:"id"`
	Operation        string        `json:"operation"`
	Config           EngineConfig  `json:"config"`
	Flake            Flake         `json:"flake"`
	Targets          []string      `json:"targets,omitempty"`
	OutLink          string        `json:"out_link,omitempty"`
	App              string        `json:"app,omitempty"`
	Rebuild          bool          `json:"rebuild,omitempty"`
	SkipCached       bool          `json:"skip_cached,omitempty"`
	AllOutputs       bool          `json:"all_outputs,omitempty"`
	AttributePaths   [][]string    `json:"attribute_paths,omitempty"`
	MaxResponseBytes int           `json:"max_response_bytes,omitempty"`
	Presentation     *Presentation `json:"presentation,omitempty"`
}
type envelope struct {
	Signal              int             `json:"signal"`
	CancellationGraceMS int             `json:"cancellation_grace_ms"`
	Type                string          `json:"type"`
	Version             int             `json:"version"`
	ID                  string          `json:"id"`
	Capabilities        []string        `json:"capabilities"`
	MaxRequestBytes     int             `json:"max_request_bytes"`
	Event               Event           `json:"event"`
	Result              json.RawMessage `json:"result"`
	Error               *Error          `json:"error"`
}
type result struct {
	Kind      string    `json:"kind"`
	Discovery Discovery `json:"discovery"`
	Manifest  Manifest  `json:"manifest"`
	Program   string    `json:"program"`
	ExitCode  int       `json:"exit_code"`
}

func (c Client) Discover(ctx context.Context, flake Flake) (Discovery, error) {
	r, err := c.invoke(ctx, request{Operation: "discover", Flake: flake})
	return r.Discovery, err
}
func (c Client) Build(ctx context.Context, r BuildRequest) (Manifest, error) {
	v, err := c.invoke(ctx, request{Operation: "build", Flake: r.Flake, Targets: r.Targets, OutLink: r.OutLink, Rebuild: r.Rebuild, SkipCached: r.SkipCached, AllOutputs: r.AllOutputs})
	return v.Manifest, err
}
func (c Client) BuildInstallables(ctx context.Context, r BuildInstallablesRequest) (Manifest, error) {
	v, err := c.invoke(ctx, request{Operation: "build_installables", Flake: r.Flake, AttributePaths: r.AttributePaths, OutLink: r.OutLink, Rebuild: r.Rebuild, SkipCached: r.SkipCached, AllOutputs: r.AllOutputs})
	return v.Manifest, err
}
func (c Client) Check(ctx context.Context, r CheckRequest) (Manifest, error) {
	v, err := c.invoke(ctx, request{Operation: "check", Flake: r.Flake, Targets: r.Targets, OutLink: r.OutLink, Rebuild: r.Rebuild, SkipCached: r.SkipCached, AllOutputs: r.AllOutputs})
	return v.Manifest, err
}
func (c Client) PrepareRun(ctx context.Context, r RunRequest) (PreparedRun, error) {
	v, err := c.invoke(ctx, request{Operation: "prepare_run", Flake: r.Flake, App: r.App, Rebuild: r.Rebuild})
	return PreparedRun{Program: v.Program, Manifest: v.Manifest}, err
}
func (c Client) FlakeCheck(ctx context.Context, f Flake) error {
	_, err := c.invoke(ctx, request{Operation: "flake_check", Flake: f})
	return err
}

type boundedBuffer struct{ bytes.Buffer }

func (b *boundedBuffer) Write(p []byte) (int, error) {
	n := len(p)
	if remaining := 64*1024 - b.Len(); remaining > 0 {
		_, _ = b.Buffer.Write(p[:min(remaining, n)])
	}
	return n, nil
}

type frame struct {
	line []byte
	err  error
}

func protocolError(message string) error { return &Error{Code: "protocol", Message: message} }
func portable(v reflect.Value) bool {
	switch v.Kind() {
	case reflect.String:
		return utf8.ValidString(v.String()) && !strings.ContainsRune(v.String(), 0)
	case reflect.Struct:
		for i := 0; i < v.NumField(); i++ {
			if !portable(v.Field(i)) {
				return false
			}
		}
	case reflect.Slice:
		for i := 0; i < v.Len(); i++ {
			if !portable(v.Index(i)) {
				return false
			}
		}
	case reflect.Pointer:
		return v.IsNil() || portable(v.Elem())
	}
	return true
}
func (c Client) invoke(ctx context.Context, r request) (out result, err error) {
	if err = ctx.Err(); err != nil {
		return out, cancellationError(ctx)
	}
	r.Type = "request"
	r.Version = ProtocolVersion
	r.ID = "1"
	r.Config = c.Config
	r.Presentation = c.Presentation
	if r.Presentation != nil && (r.Presentation.Mode != "tui" && r.Presentation.Mode != "stream") {
		return out, &Error{Code: "invalid_request", Message: "presentation mode must be tui or stream"}
	}
	if r.Presentation != nil && c.Stderr != nil {
		return out, &Error{Code: "invalid_request", Message: "presentation requires TerminalOutput instead of Stderr"}
	}
	if c.Presentation != nil {
		for _, file := range []*os.File{c.TerminalInput, c.TerminalOutput} {
			if file != nil {
				if _, statErr := file.Stat(); statErr != nil {
					return out, statErr
				}
			}
		}
	}
	maxResponse := c.MaxResponseBytes
	if maxResponse == 0 {
		maxResponse = 64 * 1024 * 1024
	}
	if maxResponse < 1024 || maxResponse > 1024*1024*1024 {
		return out, &Error{Code: "invalid_request", Message: "MaxResponseBytes must be between 1024 and 1073741824"}
	}
	r.MaxResponseBytes = maxResponse
	if !portable(reflect.ValueOf(r)) || !portable(reflect.ValueOf(c.Executable)) || !portable(reflect.ValueOf(c.Environment)) || !portable(reflect.ValueOf(c.Arguments)) {
		return out, &Error{Code: "invalid_request", Message: "strings must be valid UTF-8 without NUL"}
	}
	if c.Config.System == "" || r.Flake.Reference == "" {
		return out, &Error{Code: "invalid_request", Message: "system and flake reference are required"}
	}
	executable := c.Executable
	if executable == "" {
		executable = "nix-tools"
	}
	cmd := exec.Command(executable, append(slices.Clone(c.Arguments), "engine")...)
	cmd.Env = c.Environment
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	var stderr boundedBuffer
	cmd.Stderr = &stderr
	if c.Stderr != nil {
		cmd.Stderr = io.MultiWriter(c.Stderr, &stderr)
	}
	var stdin io.WriteCloser
	var e error
	var controlRead *os.File
	if c.Presentation != nil {
		cmd.Args = append(cmd.Args, "--interactive")
		cmd.SysProcAttr = nil
		cmd.Stdin = c.TerminalInput
		if c.TerminalInput == nil {
			cmd.Stdin = os.Stdin
		}
		cmd.Stderr = c.TerminalOutput
		if c.TerminalOutput == nil {
			cmd.Stderr = os.Stderr
		}
		var controlWrite *os.File
		controlRead, controlWrite, e = os.Pipe()
		stdin = controlWrite
		if e == nil {
			cmd.ExtraFiles = []*os.File{controlRead}
			defer controlRead.Close()
		}
	} else {
		stdin, e = cmd.StdinPipe()
	}
	if e != nil {
		return out, e
	}
	stdout, e := cmd.StdoutPipe()
	if e != nil {
		_ = stdin.Close()
		return out, e
	}
	if e = cmd.Start(); e != nil {
		_ = stdin.Close()
		_ = stdout.Close()
		return out, e
	}
	if controlRead != nil {
		_ = controlRead.Close()
	}
	grace := c.CancelGrace
	if grace <= 0 {
		grace = 3 * time.Second
	}
	kill := func() {
		if c.Presentation != nil {
			_ = cmd.Process.Kill()
		} else {
			_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
		}
		_ = stdin.Close()
		_ = stdout.Close()
	}
	stopped := make(chan struct{})
	frames := make(chan frame)
	go func() {
		scanner := bufio.NewScanner(stdout)
		scanner.Buffer(make([]byte, min(4096, maxResponse+1)), maxResponse+1)
		scanner.Split(func(data []byte, atEOF bool) (int, []byte, error) {
			if i := bytes.IndexByte(data, '\n'); i >= 0 {
				return i + 1, data[:i], nil
			}
			if atEOF && len(data) > 0 {
				return 0, nil, io.ErrUnexpectedEOF
			}
			return 0, nil, nil
		})
		for scanner.Scan() {
			p := slices.Clone(scanner.Bytes())
			select {
			case frames <- frame{line: p}:
			case <-stopped:
				return
			}
		}
		scanErr := scanner.Err()
		if scanErr == nil {
			scanErr = io.EOF
		}
		select {
		case frames <- frame{err: scanErr}:
		case <-stopped:
		}
	}()
	defer close(stopped)
	defer func() {
		if err != nil {
			kill()
		}
		waited := make(chan error, 1)
		go func() { waited <- cmd.Wait() }()
		var waitErr error
		select {
		case waitErr = <-waited:
		case <-time.After(grace):
			kill()
			waitErr = <-waited
		}
		if err == nil && waitErr != nil {
			err = &Error{Code: "engine_exit", Message: fmt.Sprintf("%v: %s", waitErr, stderr.String()), Cause: waitErr}
		}
	}()
	cancelled := false
	contextDone := ctx.Done()
	var deadline <-chan time.Time
	var timer *time.Timer
	defer func() {
		if timer != nil {
			timer.Stop()
		}
	}()
	read := func() (envelope, error) {
		for {
			select {
			case <-contextDone:
				cancelled = true
				contextDone = nil
				go func() {
					_ = json.NewEncoder(stdin).Encode(struct {
						Type    string `json:"type"`
						Version int    `json:"version"`
						ID      string `json:"id"`
						Signal  int    `json:"signal"`
					}{"cancel", 1, "1", int(cancellationSignal(ctx))})
				}()
				timer = time.NewTimer(grace)
				deadline = timer.C
			case <-deadline:
				return envelope{}, cancellationError(ctx)
			case f := <-frames:
				if f.err != nil {
					if cancelled {
						return envelope{}, cancellationError(ctx)
					}
					return envelope{}, protocolError("engine stream ended before result: " + f.err.Error())
				}
				var v envelope
				if !utf8.Valid(f.line) || json.Unmarshal(f.line, &v) != nil {
					return v, protocolError("invalid JSON frame")
				}
				if v.Version != ProtocolVersion {
					return v, protocolError("unsupported protocol version")
				}
				return v, nil
			}
		}
	}
	hello, e := read()
	if e != nil {
		return out, e
	}
	if hello.Type != "hello" || !slices.Contains(hello.Capabilities, r.Operation) {
		return out, protocolError("engine does not advertise requested capability")
	}
	if c.Presentation != nil && !slices.Contains(hello.Capabilities, "interactive_presentation") {
		return out, protocolError("engine does not advertise interactive presentation")
	}
	if r.SkipCached && !slices.Contains(hello.Capabilities, "skip_cached") {
		return out, protocolError("engine does not advertise skip_cached capability")
	}
	if r.AllOutputs && !slices.Contains(hello.Capabilities, "all_outputs") {
		return out, protocolError("engine does not advertise all_outputs capability")
	}
	if r.Config.Limits.MaxJobs != nil && !slices.Contains(hello.Capabilities, "max_jobs") {
		return out, protocolError("engine does not advertise max_jobs capability")
	}
	if hello.CancellationGraceMS < 0 || hello.CancellationGraceMS > 60000 {
		return out, protocolError("invalid cancellation grace")
	}
	if hello.CancellationGraceMS > 0 {
		grace = max(grace, time.Duration(hello.CancellationGraceMS)*time.Millisecond+time.Second)
	}
	if cancelled {
		return out, cancellationError(ctx)
	}
	if r.Rebuild && !slices.Contains(hello.Capabilities, "rebuild") {
		return out, protocolError("engine does not advertise rebuild capability")
	}
	payload, _ := json.Marshal(r)
	if hello.MaxRequestBytes > 0 && len(payload)+1 > hello.MaxRequestBytes {
		return out, &Error{Code: "invalid_request", Message: "request exceeds engine frame limit"}
	}
	written := make(chan error, 1)
	go func() { _, writeErr := stdin.Write(append(payload, '\n')); written <- writeErr }()
	select {
	case e = <-written:
		if e != nil {
			return out, e
		}
	case <-ctx.Done():
		return out, cancellationError(ctx)
	}
	for {
		message, e := read()
		if e != nil {
			return out, e
		}
		if message.ID != r.ID {
			return out, protocolError("unexpected request ID")
		}
		switch message.Type {
		case "progress":
			if c.OnEvent != nil {
				c.OnEvent(message.Event)
			}
		case "error":
			if message.Error == nil || (message.Error.Code == "" && message.Error.Category == "") {
				return out, protocolError("missing error payload")
			}
			if message.Error.Manifest != nil {
				out.Manifest = *message.Error.Manifest
			}
			if ctx.Err() != nil {
				message.Error.Cause = ctx.Err()
			}
			return out, message.Error
		case "result":
			if json.Unmarshal(message.Result, &out) != nil || out.Kind != r.Operation {
				return out, protocolError("invalid or mismatched result")
			}
			var fields map[string]json.RawMessage
			_ = json.Unmarshal(message.Result, &fields)
			required := "manifest"
			switch r.Operation {
			case "discover":
				required = "discovery"
			case "flake_check":
				required = "exit_code"
			}
			if len(fields[required]) == 0 || bytes.Equal(fields[required], []byte("null")) {
				return out, protocolError("missing result payload")
			}
			if r.Operation == "flake_check" && out.ExitCode != 0 && out.Manifest.Outcome == "" {
				return out, &Error{Code: "flake_check", Message: "flake validation failed", Status: out.ExitCode}
			}
			switch r.Operation {
			case "build", "check", "build_installables", "prepare_run", "flake_check":
				if r.Operation == "flake_check" && out.Manifest.Outcome == "" {
					break
				}
				if out.Manifest.Outcome != "success" {
					if out.Manifest.Outcome != "cancelled" && out.Manifest.Outcome != "failed" {
						return out, protocolError("invalid manifest outcome")
					}
					status := 1
					if out.Manifest.Outcome == "cancelled" {
						status = 130
						if message.Signal > 0 && message.Signal <= 64 {
							status = 128 + message.Signal
						} else if ctx.Err() != nil {
							status = 128 + int(cancellationSignal(ctx))
						}
					}
					return out, &Error{Code: out.Manifest.Outcome, Status: status, Message: "realization did not succeed", Manifest: &out.Manifest, Cause: func() error {
						if ctx.Err() != nil {
							return ctx.Err()
						}
						return nil
					}()}
				}
				if r.Operation == "prepare_run" && (out.Program == "" || !portable(reflect.ValueOf(out.Program))) {
					return out, protocolError("invalid prepared program")
				}
			}
			if ctx.Err() != nil {
				return out, cancellationError(ctx)
			}
			// A completed response must be followed by process exit, never another request.
			_ = stdin.Close()
			select {
			case f := <-frames:
				if !errors.Is(f.err, io.EOF) {
					return out, protocolError("trailing data after result")
				}
			case <-time.After(grace):
				return out, protocolError("engine did not exit after result")
			case <-ctx.Done():
				return out, cancellationError(ctx)
			}
			return out, nil
		default:
			return out, protocolError("unknown envelope type: " + message.Type)
		}
	}
}
