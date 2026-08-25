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
          sources = framework.mkRustSources {
            root = ./.;
            cargoFiles = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates/framework-demo/Cargo.toml
            ];
            productionFiles = ./crates/framework-demo/src;
            checkFiles = pkgs.lib.fileset.unions [
              ./crates/framework-demo/src
              ./crates/framework-demo/tests
            ];
          };
          craneLib = crane.mkLib pkgs;
        in
        framework.mkRustWorkspace {
          inherit pkgs craneLib sources;
          name = "framework-demo";
          packageName = "framework-demo";
          binaryName = "framework-demo";
          cargoVendorDir = craneLib.vendorCargoDeps { cargoLock = ./Cargo.lock; };
          cargoBuildExtraArgs = "--package framework-demo --bin framework-demo --locked";
          cargoClippyExtraArgs = "--package framework-demo --all-targets --locked -- -D warnings";
          cargoTestExtraArgs = "--package framework-demo --all-targets --locked";
        };
    in
    {
      packages = forAllSystems (system: (targetsFor system).packages);
      checks = forAllSystems (system: (targetsFor system).checks);
      apps = forAllSystems (system: (targetsFor system).apps);
    };
}
