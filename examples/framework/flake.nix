{
  description = "Minimal nix-tools framework consumer";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      nixpkgs,
      crane,
      ...
    }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      targetsFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          framework = import ../../nix/framework { inherit (pkgs) lib; };
          coneSources = framework.mkRustConeSources {
            root = ./.;
            workspaceFiles = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
            ];
            memberPaths = {
              framework-demo = "crates/framework-demo";
              unrelated = "crates/unrelated";
            };
            cones = {
              framework-demo = [ "framework-demo" ];
              unrelated = [ "unrelated" ];
            };
            memberSources = {
              framework-demo = {
                production = ./crates/framework-demo/src;
                check = pkgs.lib.fileset.unions [
                  ./crates/framework-demo/src
                  ./crates/framework-demo/tests
                ];
              };
              unrelated = {
                production = ./crates/unrelated/src;
                check = ./crates/unrelated/src;
              };
            };
          };
          craneLib = crane.mkLib pkgs;
        in
        framework.mkRustPackageSet {
          packageConfigs = {
            framework-demo = {
              inherit pkgs craneLib;
              name = "framework-demo";
              binaryName = "framework-demo";
              cone = coneSources.framework-demo;
              cargoVendorDir = craneLib.vendorCargoDeps { cargoLock = ./Cargo.lock; };
              cargoBuildExtraArgs = "--package framework-demo --bin framework-demo";
              cargoClippyExtraArgs = "--package framework-demo --all-targets -- -D warnings";
              cargoTestExtraArgs = "--package framework-demo --all-targets";
            };
            unrelated = {
              inherit pkgs craneLib;
              name = "unrelated";
              binaryName = "unrelated";
              cone = coneSources.unrelated;
              cargoVendorDir = craneLib.vendorCargoDeps { cargoLock = ./Cargo.lock; };
              cargoBuildExtraArgs = "--package unrelated --bin unrelated";
              cargoClippyExtraArgs = "--package unrelated --all-targets -- -D warnings";
              cargoTestExtraArgs = "--package unrelated --all-targets";
            };
          };
          defaultPackageName = "framework-demo";
        };
    in
    {
      packages = forAllSystems (system: (targetsFor system).packages);
      checks = forAllSystems (system: (targetsFor system).checks);
      apps = forAllSystems (system: (targetsFor system).apps);
    };
}
