{
  description = "Reusable tooling for fast, reproducible Nix repositories";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
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
      framework = import ./nix/framework { lib = nixpkgs.lib; };
      bun2nixLib = import ./nix/bun2nix { lib = nixpkgs.lib; };
      publicLib =
        framework
        // bun2nixLib
        // {
          mkBun = pkgs: import ./nix/bun { inherit pkgs; };
        };

      outputsFor =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          inherit (pkgs) lib;
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          memberPaths = {
            bun2nix = "crates/bun2nix";
            nix-tools = "crates/nix-tools";
            nix-tools-cache = "crates/nix-tools-cache";
            nix-tools-core = "crates/nix-tools-core";
            nix-tools-engine = "crates/nix-tools-engine";
          };
          cones = {
            bun2nix = [ "bun2nix" ];
            nix-tools-core = [ "nix-tools-core" ];
            nix-tools-engine = [
              "nix-tools-engine"
              "nix-tools-core"
            ];
            nix-tools-cache = [
              "nix-tools-cache"
              "nix-tools-core"
            ];
            nix-tools = [
              "nix-tools"
              "nix-tools-engine"
              "nix-tools-core"
            ];
          };
          sourceFor =
            path:
            let
              member = ./. + "/${path}";
              common = craneLib.fileset.commonCargoSources member;
              separateTests = lib.fileset.unions (
                [ (lib.fileset.fileFilter (file: lib.hasSuffix "_test.rs" file.name) member) ]
                ++ lib.optional (builtins.pathExists (member + "/tests")) (member + "/tests")
              );
            in
            {
              production = lib.fileset.difference common separateTests;
              check = common;
            };
          memberSources = lib.mapAttrs (_: sourceFor) memberPaths;
          coneSources = framework.mkRustConeSources {
            root = ./.;
            workspaceFiles = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./rust-toolchain.toml
            ];
            inherit memberPaths cones memberSources;
            extraCompileInputs.bun2nix.check = [ ./crates/bun2nix/tests/fixtures ];
          };
          packageConfig = name: exposePackage: {
            inherit
              pkgs
              craneLib
              name
              exposePackage
              ;
            exposeApp = exposePackage;
            packageName = name;
            binaryName = name;
            cone = coneSources.${name};
            cargoBuildExtraArgs = "--package ${name}";
            cargoClippyExtraArgs = "--package ${name} --all-targets -- -D warnings";
            cargoTestExtraArgs = "--package ${name} --all-targets";
            cargoFmtExtraArgs = "--all -- --check";
            packageArgs = lib.optionalAttrs exposePackage {
              doInstallCargoArtifacts = false;
              meta = {
                mainProgram = name;
                description =
                  if name == "bun2nix" then
                    "Generate optimized Nix inputs and workspace plans from bun.lock"
                  else
                    "Build, check, and run flakes through the nix-tools engine";
              };
            };
          };
          rustTargets = framework.mkRustPackageSet {
            packageConfigs = {
              bun2nix = (packageConfig "bun2nix" true) // {
                testArgs.nativeBuildInputs = [ bun ];
              };
              nix-tools = packageConfig "nix-tools" true;
              nix-tools-cache = (packageConfig "nix-tools-cache" false) // {
                testArgs.nativeBuildInputs = [
                  pkgs.nix
                  pkgs.openssl
                ];
              };
              nix-tools-core = (packageConfig "nix-tools-core" false) // {
                testArgs.nativeBuildInputs = [ pkgs.util-linux ];
              };
              nix-tools-engine = packageConfig "nix-tools-engine" false;
            };
            defaultPackageName = "nix-tools";
          };
          rustConesValid = framework.validateRustCones {
            root = ./.;
            inherit memberPaths cones;
          };
          bun2nix = rustTargets.packages.bun2nix;
          nixTools = rustTargets.packages.nix-tools;
          nixToolsDev = pkgs.runCommand "nix-tools-dev-cli" { } ''
            mkdir -p "$out/bin"
            ln -s ${lib.getExe nixTools} "$out/bin/nix-tools"
            ln -s nix-tools "$out/bin/nt"
          '';
          bun = publicLib.mkBun pkgs;
          frameworkEval = import ./nix/framework/tests/default.nix { inherit lib; };
          bunExampleNix = pkgs.callPackage ./examples/bun-monorepo/bun.nix { };
          bunExampleCaches = publicLib.mkBunCaches {
            inherit pkgs;
            bunNix = bunExampleNix;
            bun2nix = bun2nix;
          };
          bunCorpusNix = pkgs.callPackage ./crates/bun2nix/tests/fixtures/corpus/registry/bun.nix { };
          bunCorpusCaches = publicLib.mkBunCaches {
            inherit pkgs bun2nix;
            bunNix = bunCorpusNix;
          };
          bunCorpusSource = publicLib.mkSourceCone {
            root = ./crates/bun2nix/tests/fixtures/corpus/registry;
            paths = [ ./crates/bun2nix/tests/fixtures/corpus/registry ];
          };
          bunCorpusProduction = publicLib.mkOfflineBunWorkspace {
            inherit pkgs bun;
            name = "bun2nix-registry-corpus-production";
            src = bunCorpusSource;
            bunDeps = bunCorpusCaches.productionWorkspaceCaches.corpus-registry;
            workspaceName = "corpus-registry";
            production = true;
            installPhase = ''
              find node_modules -type l -printf '%P -> %l\n' | sort > actual-links.txt
              diff --unified ${./crates/bun2nix/tests/fixtures/corpus/registry/production-links.txt} actual-links.txt
              touch "$out"
            '';
          };
          nixSource = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.fileFilter (file: file.hasExt "nix" && file.name != "bun.nix") ./.;
          };
          formatter = pkgs.writeShellScriptBin "nix-tools-format" ''
            exec ${pkgs.nixfmt-tree}/bin/treefmt --excludes '**/bun.nix' "$@"
          '';
        in
        {
          packages = rustTargets.packages // {
            inherit bun;
          };

          checks =
            rustTargets.checks
            // {
              benchmark-harness =
                pkgs.runCommand "nix-tools-benchmark-harness-tests"
                  {
                    nativeBuildInputs = [ pkgs.python3 ];
                  }
                  ''
                    cd ${./benchmarks}
                    PYTHONDONTWRITEBYTECODE=1 python test_benchmark.py
                    touch "$out"
                  '';
              bun2nix-nix-eval = import ./nix/bun2nix/tests/check.nix { inherit pkgs; };
              bun-example-eval =
                assert builtins.length (builtins.attrNames bunExampleCaches.production.shards) == 3;
                pkgs.runCommand "bun-monorepo-example-eval" { } "touch $out";
              bun-example-generated =
                pkgs.runCommand "bun-monorepo-example-generated"
                  {
                    nativeBuildInputs = [ bun2nix ];
                  }
                  ''
                    bun2nix --lock-file ${./examples/bun-monorepo/bun.lock} --output generated.nix
                    diff --unified ${./examples/bun-monorepo/bun.nix} generated.nix
                    touch "$out"
                  '';
              dev-shell-cli-aliases =
                pkgs.runCommand "nix-tools-dev-shell-cli-aliases"
                  {
                    nativeBuildInputs = [ nixToolsDev ];
                  }
                  ''
                    nix-tools --help >/dev/null
                    nt --help >/dev/null
                    test "$(readlink "$(command -v nt)")" = nix-tools
                    test ! -e ${nixToolsDev}/bin/nixtools
                    touch "$out"
                  '';
              framework-eval =
                assert frameworkEval && rustConesValid;
                pkgs.runCommand "nix-tools-framework-eval" { } "touch $out";
              nix-fmt = pkgs.runCommand "nix-tools-nix-fmt" { nativeBuildInputs = [ formatter ]; } ''
                ${lib.getExe formatter} --ci ${nixSource}
                touch "$out"
              '';
            }
            // lib.optionalAttrs (system == "x86_64-linux") {
              bun-corpus-production = bunCorpusProduction;
            };

          apps = {
            default = {
              type = "app";
              program = "${nixTools}/bin/nix-tools";
            };
            bun2nix = {
              type = "app";
              program = "${bun2nix}/bin/bun2nix";
            };
            nix-tools = {
              type = "app";
              program = "${nixTools}/bin/nix-tools";
            };
          };

          devShells.default = pkgs.mkShell {
            packages = [
              rustToolchain
              bun
              nixToolsDev
              pkgs.nixfmt-tree
            ];
          };

          inherit formatter;
        };
    in
    {
      lib = publicLib;
      overlays.default = final: _: {
        inherit (self.packages.${final.system}) bun2nix;
        bun-current = self.packages.${final.system}.bun;
        nix-tools = self.packages.${final.system}.nix-tools;
      };
      packages = forAllSystems (system: (outputsFor system).packages);
      checks = forAllSystems (system: (outputsFor system).checks);
      apps = forAllSystems (system: (outputsFor system).apps);
      devShells = forAllSystems (system: (outputsFor system).devShells);
      formatter = forAllSystems (system: (outputsFor system).formatter);
    };
}
