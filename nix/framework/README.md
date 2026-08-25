# Nix framework

`default.nix` exports small functions that return ordinary flake target attrsets:

- `mkRustSources` turns caller-defined Cargo, production, check, and extra filesets into two source trees.
- `mkRustWorkspace` builds a package from the production tree and independent `fmt`, `clippy`, and `test` checks from the check tree. All compilation targets share one Crane `cargoArtifacts` derivation.
- `mkApp` creates a conventional Nix app from a package and binary name.

The framework does not import Nixpkgs, select a toolchain, or inspect repository paths. Consumers provide `pkgs`, a configured `craneLib`, sources, names, Cargo arguments, and optional derivation arguments, then merge `targets.packages`, `targets.checks`, and `targets.apps` with their own attrsets.
