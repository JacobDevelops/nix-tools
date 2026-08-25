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

  hiddenTargets = framework.mkRustWorkspace {
    pkgs = { };
    inherit craneLib;
    name = "library-only";
    sources = {
      production = "production-source";
      check = "check-source";
    };
    exposePackage = false;
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

  coneSources = framework.mkRustConeSources {
    root = ./fixture;
    workspaceFiles = lib.fileset.unions [
      ./fixture/Cargo.toml
      ./fixture/Cargo.lock
    ];
    memberPaths = {
      demo = "crates/demo";
      unrelated = "crates/unrelated";
    };
    cones = {
      demo = [ "demo" ];
      unrelated = [ "unrelated" ];
    };
    memberSources = {
      demo = {
        production = ./fixture/crates/demo/src;
        check = lib.fileset.unions [
          ./fixture/crates/demo/src
          ./fixture/crates/demo/tests
        ];
      };
      unrelated = {
        production = ./fixture/crates/unrelated/src;
        check = ./fixture/crates/unrelated/src;
      };
    };
    extraCompileInputs.demo.check = [ ./fixture/demo-data ];
  };

  packageSet = framework.mkRustPackageSet {
    packageConfigs = {
      demo = {
        pkgs = { };
        inherit craneLib;
        name = "demo";
        binaryName = "demo";
        cone = coneSources.demo;
      };
      unrelated = {
        pkgs = { };
        inherit craneLib;
        name = "unrelated";
        binaryName = "unrelated";
        cone = coneSources.unrelated;
      };
    };
    defaultPackageName = "demo";
  };

  duplicateTargets = builtins.tryEval (
    (framework.mergeTargets [
      { packages.demo = "first"; }
      { packages.demo = "second"; }
    ]).packages
  );
  conesMatchCargo = framework.validateRustCones {
    root = ./fixture;
    memberPaths = {
      demo = "crates/demo";
      unrelated = "crates/unrelated";
    };
    cones = {
      demo = [ "demo" ];
      unrelated = [ "unrelated" ];
    };
  };
  oversizedCone = builtins.tryEval (
    framework.validateRustCones {
      root = ./fixture;
      memberPaths = {
        demo = "crates/demo";
        unrelated = "crates/unrelated";
      };
      cones = {
        demo = [
          "demo"
          "unrelated"
        ];
        unrelated = [ "unrelated" ];
      };
    }
  );
in
assert builtins.pathExists "${sources.production}/crates/demo/src/main.rs";
assert !(builtins.pathExists "${sources.production}/crates/demo/tests/smoke.rs");
assert builtins.pathExists "${sources.check}/crates/demo/tests/smoke.rs";
assert builtins.pathExists "${coneSources.demo.sources.production}/crates/demo/src/main.rs";
assert builtins.pathExists "${coneSources.demo.sources.production}/crates/demo/Cargo.toml";
assert builtins.pathExists "${coneSources.demo.sources.production}/crates/unrelated/Cargo.toml";
assert !(builtins.pathExists "${coneSources.demo.sources.production}/crates/unrelated/src/lib.rs");
assert !(builtins.pathExists "${coneSources.demo.sources.production}/demo-data/message.txt");
assert builtins.pathExists "${coneSources.demo.sources.check}/demo-data/message.txt";
assert lib.hasInfix "crates/unrelated" coneSources.demo.preConfigure;
assert lib.hasInfix "custom/library.rs" coneSources.demo.preConfigure;
assert lib.hasInfix "commands/tool.rs" coneSources.demo.preConfigure;
assert lib.hasInfix "checks/integration.rs" coneSources.demo.preConfigure;
assert lib.hasInfix "performance/benchmark.rs" coneSources.demo.preConfigure;
assert lib.hasInfix "samples/example.rs" coneSources.demo.preConfigure;
assert lib.hasInfix "support/build-script.rs" coneSources.demo.preConfigure;
assert packageSet.packages.demo.args.src == coneSources.demo.sources.production;
assert packageSet.checks.demo-clippy.args.src == coneSources.demo.sources.check;
assert packageSet.packages.default.kind == "package";
assert packageSet.apps.default.program == "/nix/store/demo-package/bin/demo";
assert !duplicateTargets.success;
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
assert hiddenTargets.packages == { };
assert hiddenTargets.apps == { };
assert conesMatchCargo;
assert !oversizedCone.success;
true
