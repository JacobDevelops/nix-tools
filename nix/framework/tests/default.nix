{ lib }:
let
  framework = import ../default.nix { inherit lib; };

  mkDerivation = kind: args: {
    inherit kind args;
    __toString = _: "/nix/store/${args.pname}-${kind}";
  };

  craneLib = {
    buildDepsOnly = mkDerivation "deps";
    buildPackage = mkDerivation "package";
    cargoClippy = mkDerivation "clippy";
    cargoFmt = mkDerivation "fmt";
    cargoTest = mkDerivation "test";
  };

  targets = framework.mkRustWorkspace {
    pkgs = { };
    inherit craneLib;
    name = "demo";
    packageName = "demo-cli";
    binaryName = "demo";
    sources = {
      production = "production-source";
      check = "check-source";
    };
  };

  sources = framework.mkRustSources {
    root = ./fixture;
    cargoFiles = lib.fileset.unions [
      ./fixture/Cargo.toml
      ./fixture/Cargo.lock
      ./fixture/crates/demo/Cargo.toml
    ];
    productionFiles = ./fixture/crates/demo/src;
    checkFiles = lib.fileset.unions [
      ./fixture/crates/demo/src
      ./fixture/crates/demo/tests
    ];
  };
in
assert builtins.pathExists "${sources.production}/crates/demo/src/main.rs";
assert !(builtins.pathExists "${sources.production}/crates/demo/tests/smoke.rs");
assert builtins.pathExists "${sources.check}/crates/demo/tests/smoke.rs";
assert targets.package.args.src == "production-source";
assert targets.package.args.doCheck == false;
assert targets.checks.fmt.args.src == "check-source";
assert targets.checks.clippy.args.src == "check-source";
assert targets.checks.test.args.src == "check-source";
assert targets.checks.test.args.doCheck == true;
assert targets.package.args.cargoArtifacts.kind == "deps";
assert targets.checks.clippy.args.cargoArtifacts.kind == "deps";
assert targets.checks.test.args.cargoArtifacts.kind == "deps";
assert targets.packages.default.kind == "package";
assert targets.packages.demo-cli.kind == "package";
assert targets.apps.default.program == "/nix/store/demo-package/bin/demo";
assert targets.apps.demo-cli.program == "/nix/store/demo-package/bin/demo";
true
