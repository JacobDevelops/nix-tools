# CLI services

The reference binary has three primary commands:

```sh
nix-tools build                 # every package
nix-tools build api             # one package
nix-tools check                 # every check
nix-tools check api             # every api:* check
nix-tools check api:test        # one scoped check
nix-tools run api:dev -- --port 3000
nix-tools run api:gen
```

Every operation goes through `nix-tools-engine`. Builds and checks submit all selected roots together so evaluation batches, derivation deduplication, cache probes, and dependency scheduling work across the whole request. `run` realizes the derivations carried by the app program's Nix string context before executing it.

Progress defaults to a live dependency map for `build`, `check`, and the realization stage of `run`. Realization reads the `nix build` activity stream, so a job turns running when Nix actually starts building or substituting it and carries its own elapsed time, while the header tracks settled jobs and total wall clock; derivations Nix never reports stay queued until they settle from the cache. Arrow keys or `j`/`k` move through jobs, the selected pane adds timing, transfer progress, and reverse dependencies, `?` opens help, and `q` requests cancellation. The TUI restores the terminal before a realized app starts and falls back to stream output without a usable terminal. `--output=stream` selects stable line-oriented output explicitly. `plan` remains non-interactive because its standard output is a machine-readable JSON contract, so it does not expose the output option.

The binary explicitly trusts `cache.nixos.org`. Additional caches require paired flags so a URL cannot be enabled without its signing key:

```sh
nix-tools \
  --substituter https://cache.example.com \
  --trusted-public-key 'cache.example.com-1:...' \
  build
```

## Repository CLIs

This repository uses the service targets itself. From its root:

```sh
cargo run -p nix-tools -- check nix-tools:test
cargo run -p nix-tools -- check nix-tools
cargo run -p nix-tools -- check nix:fmt
cargo run -p nix-tools -- run nix:fmt
```

`check nix:fmt` verifies Nix formatting without editing files; `run nix:fmt`
formats Nix files. Each Rust crate owns its `fmt`, `clippy`, and `test` checks.
Other checks are grouped under `nix`, `benchmarks`, `bun-example`, `bun-corpus`,
`dev-shell`, and `release-cache`; Bun's Nix API check belongs to `bun2nix`.

`ServiceTarget` parses a required `service:job` and exposes `service()`, `job()`, and
`output_name()`. Pass its output name to `RuntimeCommand::Run.app`. Use
`ServiceCheckSelector` as `SelectedCheckCommand.selector` to support both
`check service` and `check service:check` without writing another selector.
The low-level runtime continues to accept exact Nix output names for custom callers.

The selector is the literal Nix attribute name: `web:dev` selects
`apps.<system>."web:dev"`; `web:lint` selects `checks.<system>."web:lint"`.
Service boundaries are exact, so `check api` does not select `api-worker:lint`.
Names start with an ASCII letter or digit and otherwise contain letters, digits,
`_`, `.`, or `-`. Unknown checks fail rather than selecting everything.

Use the language-independent [`mkServiceTargets`](../nix/framework/README.md)
helper to define a service's optional package, checks, and jobs. It returns
ordinary packages/checks/apps attrsets that `mergeTargets` combines across services.
Checks are Nix derivations; jobs are Nix apps created with `mkApp` or supplied directly.
A job such as `web:typecheck` must be declared under `jobs` to use `run`;
declaring it under `checks` enables `check web:typecheck`.

Migration: the reference CLI now requires `service:job` for `run` and selects
colon-named checks. Existing `service-job` outputs need renaming or wrapping with
`mkServiceTargets`; a bare app needs an explicit job name. Existing Rust/Bun
builders remain available and their returned derivations can be passed to this
helper. An unscoped `check` still checks every output, including legacy names.

The binary is not the extension surface. A repository CLI depends on the Rust crates and composes the typed engine, selection, progress, and output services inside its own command tree. It can rename or omit the standard commands and add deployment, database, mobile, infrastructure, or any other repository-specific operations without changing the engine.

`nix-tools-core` owns process safety and scheduling. `nix-tools-engine` owns Nix evaluation and realization. `nix-tools` supplies the standard command services. Keeping those layers separate prevents a repository's Clap structure from becoming a public compatibility constraint.
