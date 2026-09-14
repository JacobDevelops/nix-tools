# nix-tools Go SDK

Pure Go client for protocol v1, with no cgo or third-party Go dependencies. Linux and macOS are supported. Each operation starts `nix-tools engine`; `Client.Executable` selects a pinned engine binary. The SDK and engine should come from the same immutable repository revision, including through the Nix package pair.

Set `Client.Presentation = &nixtools.Presentation{Mode: "tui", Title: "Build"}` to use the same Rust renderer as the reference CLI. `tui` falls back to its stream renderer when redirected; `stream` explicitly selects line output. The engine retains the foreground terminal through stdin/stderr while a separate inherited file descriptor carries requests and cancellation, so terminal input cannot corrupt JSON. `TerminalInput` and `TerminalOutput` select files when embedding; they default to `os.Stdin` and `os.Stderr`. Leave `Stderr` unset with presentation, and disable a duplicate `OnEvent` renderer. Headless clients remain unchanged and can render events themselves.

```go
client := nixtools.Client{
    Executable: "/path/to/nix-tools",
    Config: nixtools.EngineConfig{System: "x86_64-linux"},
    OnEvent: renderProgress,
}
flake := nixtools.Flake{Reference: ".", WorkingDirectory: repo}
targets, err := client.Discover(ctx, flake)
manifest, err := client.Build(ctx, nixtools.BuildRequest{
    Flake: flake, Targets: selectedPackages, OutLink: "result",
})
```

The caller selects from `targets.Packages`, `Checks`, and `Apps`. `Build` and `Check` select all targets when `Targets` is empty. `Rebuild` bypasses cache reuse for selected roots. `FlakeCheck` runs full flake validation, which differs from realizing selected checks. `EngineConfig` carries the Nix executable, system, trusted substituters, graph mode, and resource budgets. Omitted budget fields use engine defaults.

`BuildInstallables` accepts `AttributePaths [][]string`, such as `{{"legacyPackages", "x86_64-linux", "android"}}`, preserving each attribute component without flattening caller input into Nix syntax. It shares build evaluation, progress, realization, and manifests. `Limits.MaxJobs` is an optional positive job budget; nil preserves the Nix daemon default. `SkipCached` on build/check/installables intentionally leaves trusted remote roots unmaterialized and marks them `cached_remote`; an explicit `OutLink` still materializes them for usable links. False preserves ordinary realization and is independent of `Rebuild`. Provider-specific cache publication and CI result-file layouts remain caller-owned.

`AllOutputs` on build/check/installables selects every derivation output, matching CI workflows that previously built `drv^*`. Its default false preserves normal `meta.outputsToInstall` selection. It does not force a rebuild or alter cache policy.

`PrepareRun` returns a program and manifest without launching the application. Call `prepared.Exec(arguments, environment, directory)` for a CLI's final handoff: it replaces the Go process and preserves terminal access, signals, and exit status. Failure restores the working directory; because changing directories is process-wide, call it only at the final handoff after concurrent work has stopped. `prepared.Execute(ctx, AppOptions{...})` supervises an isolated child process group and accepts custom streams. Use `Exec` for interactive terminal applications; `Execute` is for programmatically managed input and output.

Engine environment (`Client.Environment`) and app environment are separate. Nil inherits the parent environment; an empty slice supplies an empty environment. Application arguments never enter the JSON protocol, so native argument bytes are preserved. Protocol strings reject invalid UTF-8 and NUL before launching the engine.

The handshake checks protocol version and required capabilities before sending a request. Additive fields and unknown progress event kinds are accepted; `Event.Data` retains their JSON. Unknown envelope types, mismatched IDs, malformed/truncated frames, and mismatched result kinds fail explicitly. `MaxResponseBytes` bounds each frame (default 64 MiB, configurable from 1 KiB through 1 GiB) and is sent to the engine so its serializer shares the limit. Retained engine stderr is limited to 64 KiB; `Client.Stderr` optionally streams all stderr to a caller-owned writer. Event callbacks and output writers must return promptly; they run synchronously and should not mutate the client during an operation.

Context cancellation sends a cancel envelope, allows `CancelGrace` (default three seconds) for a final manifest, then kills the engine process group. The engine's advertised cancellation cleanup bound plus one second is a minimum grace, allowing it to reap Nix process groups before escalation. `SignalCause` with `context.WithCancelCause` preserves a chosen termination signal; ordinary context cancellation sends SIGINT. Build/check failures return their manifest alongside an `*Error`; cancelled manifests remain available too. `Error` exposes stable category/code, message, optional manifest, and exit status; `ExitCode` also handles wrapped errors and context cancellation.

Run `go test -race -cover ./...`, `go vet ./...`, and `CGO_ENABLED=0 go build ./...`. Real engine integration tests use the `integration` build tag and `NIX_TOOLS_ENGINE` pointing at the built binary.
