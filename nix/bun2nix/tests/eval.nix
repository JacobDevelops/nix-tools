{
  pkgs ? import <nixpkgs> { },
}:
let
  api = import ../default.nix { lib = pkgs.lib; };

  source = name: builtins.toFile name name;

  canonical = {
    format = "bun2nix";
    workspaces = [
      "admin"
      "empty"
      "worker"
    ];
    packages = [
      [
        "common@1.0.0"
        (source "common")
        null
        null
        "npm.private.example"
      ]
      [
        "linux@1.0.0"
        (source "linux")
        [ "linux" ]
        [ "x64" ]
        "registry.npmjs.org"
      ]
      [
        "darwin@1.0.0"
        (throw "excluded Darwin source was forced")
        [ "darwin" ]
        null
        "registry.npmjs.org"
      ]
      [
        "arm@1.0.0"
        (source "arm")
        null
        [ "arm64" ]
        "registry.npmjs.org"
      ]
    ];
    groups = {
      production = [
        [
          [
            0
            2
          ]
          [ 0 ]
        ]
        [
          [ 0 ]
          [
            1
            2
            3
          ]
        ]
      ];
      check = [
        [
          [
            0
            2
          ]
          [ 0 ]
        ]
        [
          [ 0 ]
          [ 1 ]
        ]
      ];
      development = [
        [
          [
            0
            2
          ]
          [ 0 ]
        ]
      ];
    };
  };

  cacheEntryCreator = pkgs.writeShellScriptBin "bun2nix" "exit 0";
  shard = api.mkCacheShard {
    inherit pkgs;
    name = "admin";
    bun2nix = cacheEntryCreator;
    packages = [
      {
        resolution = "common@1.0.0";
        source = source "common";
        os = null;
        cpu = null;
        registry = "npm.private.example";
      }
    ];
  };
  cachePlan = api.mkBunCaches {
    inherit pkgs;
    bunNix = canonical;
    bun2nix = cacheEntryCreator;
    system = "x86_64-linux";
  };
  workspaceCaches = api.mkWorkspaceCaches {
    inherit pkgs;
    workspaces = canonical.workspaces;
    shards = {
      shared = {
        consumers = [
          "admin"
          "worker"
        ];
        path = shard;
      };
    };
  };
  sourceCone = api.mkSourceCone {
    root = ../..;
    paths = [ ../default.nix ];
  };
  workspaceOutputs = api.mkBunWorkspaceOutputs {
    inherit pkgs;
    bunNix = canonical;
    bun2nix = cacheEntryCreator;
    workspaces = {
      admin = {
        src = sourceCone;
        productionSrc = source "production-source";
        checkSrc = source "check-source";
        workspaceName = "admin";
        installRoot = ".";
        workspaceRoot = "apps/admin";
        build = "bun --version";
        test = "bun test";
        bundle = "bun build index.ts";
        run = "bun run start";
      };
    };
  };
  lifecycleWorkspace = api.mkOfflineBunWorkspace {
    inherit pkgs;
    name = "admin-lifecycle";
    src = sourceCone;
    bunDeps = workspaceCaches.admin;
    workspaceName = "admin";
    lifecycle = "run";
  };
  invalid = builtins.tryEval (
    (api.validateBunNix {
      format = "bun2nix";
      workspaces = [ "admin" ];
      packages = [
        [
          "invalid"
          (source "invalid")
          "linux"
          null
          null
        ]
      ];
      groups = {
        production = [ ];
        check = [ ];
        development = [ ];
      };
    }).format
  );
  invalidIndex = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        groups = canonical.groups // {
          production = [
            [
              [ 3 ]
              [ 0 ]
            ]
          ];
        };
      }
    )).format
  );
  invalidPackageIndex = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        groups = canonical.groups // {
          production = [
            [
              [ 0 ]
              [ 4 ]
            ]
          ];
        };
      }
    )).format
  );
  invalidWithinRowDuplicates = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        groups = canonical.groups // {
          production = [
            [
              [
                0
                0
              ]
              [
                1
                1
              ]
            ]
          ];
        };
      }
    )).format
  );
  invalidRepeatedConsumers = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        groups = canonical.groups // {
          production = [
            [
              [ 0 ]
              [ 0 ]
            ]
            [
              [ 0 ]
              [ 1 ]
            ]
          ];
        };
      }
    )).format
  );
  invalidPackageOverlap = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        groups = canonical.groups // {
          production = [
            [
              [ 0 ]
              [ 0 ]
            ]
            [
              [ 2 ]
              [ 0 ]
            ]
          ];
        };
      }
    )).format
  );
  invalidEmptyGroup = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        groups = canonical.groups // {
          production = [
            [
              [ ]
              [ ]
            ]
          ];
        };
      }
    )).format
  );
  invalidWorkspaceNames = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        workspaces = [
          "admin"
          "admin"
        ];
      }
    )).format
  );
  invalidResolutions = builtins.tryEval (
    (api.validateBunNix (
      canonical
      // {
        packages = [
          (builtins.elemAt canonical.packages 0)
          (builtins.elemAt canonical.packages 0)
        ];
        groups = {
          production = [ ];
          check = [ ];
          development = [ ];
        };
      }
    )).format
  );
  invalidSelectedSource = builtins.tryEval (
    let
      plan = api.mkBunCaches {
        inherit pkgs;
        bun2nix = cacheEntryCreator;
        system = "x86_64-linux";
        bunNix = {
          format = "bun2nix";
          workspaces = [ "admin" ];
          packages = [
            [
              "invalid-source@1.0.0"
              42
              [ "linux" ]
              null
              null
            ]
          ];
          groups = {
            production = [
              [
                [ 0 ]
                [ 0 ]
              ]
            ];
            check = [ ];
            development = [ ];
          };
        };
      };
    in
    plan.production.shards."[\"admin\"]".path.manifestText
  );
in
assert (api.validateBunNix canonical).format == "bun2nix";
assert cachePlan.production.shards."[\"admin\",\"worker\"]".resolutions == [ "common@1.0.0" ];
assert
  cachePlan.production.shards."[\"admin\",\"worker\"]".path.name == "bun-cache-b802adbe18bb69dd";
assert
  cachePlan.production.shards."[\"admin\"]".resolutions == [
    "linux@1.0.0"
    "darwin@1.0.0"
    "arm@1.0.0"
  ];
assert pkgs.lib.hasInfix "linux@1.0.0\t"
  cachePlan.production.shards."[\"admin\"]".path.manifestText;
assert
  !(pkgs.lib.hasInfix "darwin@1.0.0\t" cachePlan.production.shards."[\"admin\"]".path.manifestText);
assert
  !(pkgs.lib.hasInfix "arm@1.0.0\t" cachePlan.production.shards."[\"admin\"]".path.manifestText);
assert builtins.attrNames cachePlan.production.workspaceCaches == canonical.workspaces;
assert shard.dontFixup;
assert builtins.match ".*patchShebangs.*" shard.buildPhase != null;
assert builtins.match ".*cache-entry --out.*" shard.buildPhase != null;
assert pkgs.lib.hasInfix "registry_argument=(--registry \"$registry\")" shard.buildPhase;
assert pkgs.lib.hasInfix "common@1.0.0\t" shard.manifestText;
assert pkgs.lib.hasInfix "npm.private.example" shard.manifestText;
assert builtins.match ".*readlink -f.*" shard.buildPhase != null;
assert builtins.hasAttr "admin" workspaceCaches;
assert builtins.hasAttr "worker" workspaceCaches;
assert builtins.hasAttr "empty" workspaceCaches;
assert builtins.hasAttr "admin" workspaceOutputs.packages;
assert builtins.hasAttr "admin-test" workspaceOutputs.checks;
assert builtins.hasAttr "admin" workspaceOutputs.apps;
assert builtins.hasAttr "admin" workspaceOutputs.devShells;
assert builtins.hasAttr "admin" workspaceOutputs.productionWorkspaceCaches;
assert builtins.hasAttr "admin" workspaceOutputs.checkWorkspaceCaches;
assert builtins.hasAttr "admin" workspaceOutputs.developmentWorkspaceCaches;
assert workspaceOutputs.packages.admin.src == source "production-source";
assert workspaceOutputs.checks.admin-test.src == source "check-source";
assert
  builtins.match ".*BUN_INSTALL_CACHE_DIR.*--offline.*" workspaceOutputs.packages.admin.buildPhase
  != null;
assert pkgs.lib.hasInfix "cd ." workspaceOutputs.packages.admin.buildPhase;
assert pkgs.lib.hasInfix "--linker=isolated" workspaceOutputs.packages.admin.buildPhase;
assert pkgs.lib.hasInfix "--filter" workspaceOutputs.packages.admin.buildPhase;
assert pkgs.lib.hasInfix "--production" workspaceOutputs.packages.admin.buildPhase;
assert !(pkgs.lib.hasInfix "--production" workspaceOutputs.checks.admin-test.buildPhase);
assert pkgs.lib.hasInfix "admin" workspaceOutputs.packages.admin.buildPhase;
assert pkgs.lib.hasInfix "--ignore-scripts" workspaceOutputs.packages.admin.buildPhase;
assert pkgs.lib.hasInfix "bun2nix-no-op/bin" workspaceOutputs.packages.admin.buildPhase;
assert !(pkgs.lib.hasInfix "--ignore-scripts" lifecycleWorkspace.buildPhase);
assert pkgs.lib.hasInfix "cd apps/admin" workspaceOutputs.packages.admin.buildPhase;
assert pkgs.lib.hasInfix "cd apps/admin" workspaceOutputs.checks.admin-test.checkPhase;
assert pkgs.lib.hasInfix "cp -r ./. \"$out/\"" workspaceOutputs.packages.admin.installPhase;
assert builtins.match ".*bun build index.ts.*" workspaceOutputs.packages.admin.buildPhase != null;
assert !invalid.success;
assert !invalidIndex.success;
assert !invalidPackageIndex.success;
assert !invalidWithinRowDuplicates.success;
assert !invalidRepeatedConsumers.success;
assert !invalidPackageOverlap.success;
assert !invalidEmptyGroup.success;
assert !invalidWorkspaceNames.success;
assert !invalidResolutions.success;
assert !invalidSelectedSource.success;
"bun2nix-nix-eval-tests"
