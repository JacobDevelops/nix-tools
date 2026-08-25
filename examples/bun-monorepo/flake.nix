{
  description = "Workspace-sharded Bun monorepo with nix-tools";

  inputs = {
    nix-tools.url = "../..";
    nixpkgs.follows = "nix-tools/nixpkgs";
  };

  outputs =
    {
      nix-tools,
      nixpkgs,
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
      outputsFor =
        system:
        let
          pkgs = import nixpkgs { inherit system; };
          inherit (pkgs) lib;
          bunNix = pkgs.callPackage ./bun.nix { };
          manifests = [
            ./package.json
            ./bun.lock
            ./apps/api/package.json
            ./apps/web/package.json
            ./packages/shared/package.json
          ];
          sharedSource = ./packages/shared/src;
          apiProduction = lib.fileset.difference ./apps/api/src ./apps/api/src/index.test.ts;
          apiCheck = ./apps/api/src;
          webProduction = lib.fileset.difference ./apps/web/src ./apps/web/src/index.test.ts;
          webCheck = ./apps/web/src;
          sourceCone =
            paths:
            nix-tools.lib.mkSourceCone {
              root = ./.;
              inherit paths;
            };
          apiProductionSource = sourceCone (
            manifests
            ++ [
              sharedSource
              apiProduction
            ]
          );
          apiCheckSource = sourceCone (
            manifests
            ++ [
              sharedSource
              apiCheck
            ]
          );
          webProductionSource = sourceCone (
            manifests
            ++ [
              sharedSource
              webProduction
            ]
          );
          webCheckSource = sourceCone (
            manifests
            ++ [
              sharedSource
              webCheck
            ]
          );
          installBundle = workspace: ''
            runHook preInstall
            mkdir -p "$out/${workspace}"
            cp -r ${workspace}/dist "$out/${workspace}/"
            runHook postInstall
          '';
          workspaceOutputs = nix-tools.lib.mkBunWorkspaceOutputs {
            inherit pkgs bunNix;
            bun2nix = nix-tools.packages.${system}.bun2nix;
            bun = nix-tools.packages.${system}.bun;
            workspaces = {
              example-api = {
                src = apiCheckSource;
                productionSrc = apiProductionSource;
                checkSrc = apiCheckSource;
                workspaceRoot = "apps/api";
                build = "bun run build";
                test = "bun test";
                run = "bun dist/index.js";
                installPhase = installBundle "apps/api";
              };
              example-web = {
                src = webCheckSource;
                productionSrc = webProductionSource;
                checkSrc = webCheckSource;
                workspaceRoot = "apps/web";
                build = "bun run build";
                test = "bun test";
                run = "bun dist/index.js";
                installPhase = installBundle "apps/web";
              };
            };
          };
          cachePlan = nix-tools.lib.mkBunCaches {
            inherit pkgs bunNix;
            bun2nix = nix-tools.packages.${system}.bun2nix;
          };
          cacheShape =
            assert builtins.length (builtins.attrNames cachePlan.shards) == 3;
            assert !(builtins.pathExists "${apiProductionSource}/apps/api/src/index.test.ts");
            assert builtins.pathExists "${apiCheckSource}/apps/api/src/index.test.ts";
            pkgs.runCommand "bun-monorepo-cache-shape" { } "touch $out";
        in
        {
          packages = workspaceOutputs.packages // {
            default = workspaceOutputs.packages.example-api;
          };
          checks = workspaceOutputs.checks // {
            cache-shape = cacheShape;
          };
          apps = workspaceOutputs.apps // {
            default = workspaceOutputs.apps.example-api;
          };
          devShells = workspaceOutputs.devShells // {
            default = pkgs.mkShell { packages = [ nix-tools.packages.${system}.bun ]; };
          };
        };
    in
    {
      packages = forAllSystems (system: (outputsFor system).packages);
      checks = forAllSystems (system: (outputsFor system).checks);
      apps = forAllSystems (system: (outputsFor system).apps);
      devShells = forAllSystems (system: (outputsFor system).devShells);
    };
}
