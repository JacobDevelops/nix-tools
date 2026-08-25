{ lib }:
let
  inherit (lib) fileset;

  mkSource =
    {
      root,
      fileset,
    }:
    lib.fileset.toSource { inherit root fileset; };

  # Both source trees must contain the workspace manifests. `productionFiles`
  # deliberately excludes test-only sources when a repository can identify them.
  mkRustSources =
    {
      root,
      cargoFiles,
      productionFiles,
      checkFiles ? productionFiles,
      extraFiles ? [ ],
    }:
    {
      production = mkSource {
        inherit root;
        fileset = fileset.unions ([ cargoFiles productionFiles ] ++ extraFiles);
      };
      check = mkSource {
        inherit root;
        fileset = fileset.unions ([ cargoFiles checkFiles ] ++ extraFiles);
      };
    };

  mkApp =
    {
      package,
      binaryName,
    }:
    {
      type = "app";
      program = "${package}/bin/${binaryName}";
    };

  # `sources.production` and `sources.check` are complete Cargo source trees.
  # A shared profile in `commonArgs` is required when consumers expect every
  # target below to reuse the same dependency artifacts.
  mkRustWorkspace =
    {
      pkgs,
      craneLib,
      name,
      sources,
      version ? "0.1.0",
      packageName ? name,
      binaryName ? packageName,
      cargoBuildExtraArgs ? "--package ${packageName}",
      cargoClippyExtraArgs ? "--package ${packageName} --all-targets -- -D warnings",
      cargoTestExtraArgs ? "--package ${packageName} --all-targets",
      cargoFmtExtraArgs ? "--all -- --check",
      cargoVendorDir ? null,
      nativeBuildInputs ? [ ],
      buildInputs ? [ ],
      commonArgs ? { },
      packageArgs ? { },
      clippyArgs ? { },
      testArgs ? { },
      fmtArgs ? { },
    }:
    let
      vendorArgs = lib.optionalAttrs (cargoVendorDir != null) { inherit cargoVendorDir; };
      common =
        src:
        commonArgs
        // vendorArgs
        // {
          pname = name;
          inherit version src nativeBuildInputs buildInputs;
          strictDeps = true;
          doCheck = false;
        };
      cargoArtifacts = craneLib.buildDepsOnly (common sources.check);
      package = craneLib.buildPackage (
        (common sources.production)
        // packageArgs
        // {
          inherit cargoArtifacts cargoBuildExtraArgs;
          doCheck = false;
        }
      );
      clippy = craneLib.cargoClippy (
        (common sources.check)
        // clippyArgs
        // {
          inherit cargoArtifacts cargoClippyExtraArgs;
          doCheck = false;
          doInstallCargoArtifacts = false;
        }
      );
      test = craneLib.cargoTest (
        (common sources.check)
        // testArgs
        // {
          inherit cargoArtifacts cargoTestExtraArgs;
          doCheck = true;
          doInstallCargoArtifacts = false;
        }
      );
      fmt = craneLib.cargoFmt (
        (common sources.check)
        // fmtArgs
        // {
          inherit cargoFmtExtraArgs;
          doCheck = false;
        }
      );
      app = mkApp {
        inherit package binaryName;
      };
    in
    {
      inherit cargoArtifacts package;
      packages = {
        default = package;
        ${packageName} = package;
      };
      checks = {
        inherit fmt clippy test;
      };
      apps = {
        default = app;
        ${packageName} = app;
      };
    };
in
{
  inherit mkApp mkRustSources mkRustWorkspace;
}
