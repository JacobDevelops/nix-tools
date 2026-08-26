# CLI services

The reference binary has three primary commands:

```sh
nix-tools build                 # every package
nix-tools build api             # one package
nix-tools check                 # every check
nix-tools check api             # checks in one caller-defined scope
nix-tools check api:test        # one scoped check
nix-tools run api -- --port 3000
```

Every operation goes through `nix-tools-engine`. Builds and checks submit all selected roots together so evaluation batches, derivation deduplication, cache probes, and dependency scheduling work across the whole request. `run` realizes the derivations carried by the app program's Nix string context before executing it.

Progress defaults to a live dependency map for `build`, `check`, and the realization stage of `run`; arrow keys or `j`/`k` move through jobs, `?` opens help, and `q` requests cancellation. The TUI restores the terminal before a realized app starts and falls back to stream output without a usable terminal. `--output=stream` selects stable line-oriented output explicitly. `plan` remains non-interactive because its standard output is a machine-readable JSON contract, so it does not expose the output option.

The binary explicitly trusts `cache.nixos.org`. Additional caches require paired flags so a URL cannot be enabled without its signing key:

```sh
nix-tools \
  --substituter https://cache.example.com \
  --trusted-public-key 'cache.example.com-1:...' \
  build
```

## Repository CLIs

The binary is not the extension surface. A repository CLI depends on the Rust crates and composes the typed engine, selection, progress, and output services inside its own command tree. It can rename or omit the standard commands and add deployment, database, mobile, infrastructure, or any other repository-specific operations without changing the engine.

`nix-tools-core` owns process safety and scheduling. `nix-tools-engine` owns Nix evaluation and realization. `nix-tools` supplies the standard command services. Keeping those layers separate prevents a repository's Clap structure from becoming a public compatibility constraint.
