# Nix framework

`default.nix` exports small functions that return ordinary flake target attrsets:

- `mkRustSources` turns caller-defined Cargo, production, check, and extra filesets into two source trees.
- `mkRustConeSources` builds per-member source cones that retain every workspace manifest and stub members outside a cone before Cargo configures.
- `mkRustWorkspace` builds a package from the production tree and independent `fmt`, `clippy`, and `test` checks from the check tree. All compilation targets share one Crane `cargoArtifacts` derivation.
- `mkRustPackageSet` combines cone-backed packages; it prefixes check names with their package name and selects an optional default package/app.
- `mergeTargets` rejects duplicate package, check, app, or development-shell names rather than silently overwriting them.
- `mkApp` creates a conventional Nix app from a package and binary name.
- `mkServiceTargets` groups language-independent service packages, checks, and jobs without selecting defaults.

The framework does not import Nixpkgs, select a toolchain, or inspect repository paths. Consumers provide `pkgs`, a configured `craneLib`, sources, names, Cargo arguments, and optional derivation arguments, then merge `targets.packages`, `targets.checks`, and `targets.apps` with their own attrsets.

## Service targets

`mkServiceTargets { name; package ? null; checks ? {}; jobs ? {}; }` returns
`packages`, `checks`, and `apps`. A non-null package becomes `packages.${name}`;
check derivations become `checks."${name}:${checkName}"`; jobs become
`apps."${name}:${jobName}"`. Jobs are standard Nix app attrsets, passed through
unchanged; use `mkApp` to turn a package and binary name into an app.
No default aliases are added. A service with only a valid name returns three
empty attrsets.

Each service, check, and job name must start with an ASCII letter or digit,
followed only by ASCII letters, digits, `_`, `.`, or `-`.
Empty names, colons, whitespace, and slashes are rejected. All names are
validated when the result attrset is evaluated, even if an output is unused;
check and app values remain lazy. The optional package is inspected only when
`packages` is evaluated. The literal colon separates service membership exactly:
`api:worker-refresh-cache` and `api-worker:refresh-cache` are distinct targets.
`mergeTargets` rejects duplicate output names. Existing Rust and Bun helper
outputs retain their naming; callers can opt into this convention by passing
their package, unprefixed checks, and job apps through `mkServiceTargets`.

For example, with caller-provided `pkgs` and `framework`:

```nix
let
  report = pkgs.writeShellApplication {
    name = "report";
    runtimeInputs = [ pkgs.coreutils ];
    text = ''
      wc -l "$@"
    '';
  };
  service = framework.mkServiceTargets {
    name = "reports";
    package = report;
    checks.smoke = pkgs.runCommand "reports-smoke" { } ''
      printf 'one\ntwo\n' > input.txt
      test "$(${report}/bin/report < input.txt)" = 2
      touch "$out"
    '';
    jobs.count-lines = framework.mkApp {
      package = report;
      binaryName = "report";
    };
  };
in
framework.mergeTargets [ service ]
```

Assign the merged `packages`, `checks`, and `apps` to the corresponding flake
outputs for the caller's system. This exposes `reports`, `reports:smoke`, and
`reports:count-lines` respectively. The caller owns dependency selection and
working-directory policy: this example declares `coreutils` explicitly and
resolves input paths relative to the invoking shell's working directory.
