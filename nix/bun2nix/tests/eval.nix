{
  pkgs ? import <nixpkgs> { },
}:
let
  api = import ../default.nix { lib = pkgs.lib; };

  source = name: builtins.toFile name name;

  canonical = {
    packages = {
      "common@1.0.0" = source "common";
      "linux@1.0.0" = source "linux";
      "darwin@1.0.0" = source "darwin";
      "arm@1.0.0" = source "arm";
      workspace = source "workspace";
    };
    metadata = {
      lockfileVersion = 3;
      workspacePackages = [ "workspace" ];
      productionDependencyClosures = {
        admin = [
          "common@1.0.0"
          "linux@1.0.0"
        ];
        worker = [ "common@1.0.0" ];
      };
      checkDependencyClosures = {
        admin = [
          "common@1.0.0"
          "linux@1.0.0"
        ];
        worker = [ "common@1.0.0" ];
      };
      developmentDependencyClosures = {
        admin = [
          "common@1.0.0"
          "linux@1.0.0"
        ];
        worker = [ "common@1.0.0" ];
      };
      packages = {
        "common@1.0.0" = {
          source = "npm";
          local = false;
          os = null;
          cpu = null;
          registry = "npm.private.example";
        };
        "linux@1.0.0" = {
          source = "npm";
          local = false;
          os = [ "linux" ];
          cpu = [ "x64" ];
          registry = "registry.npmjs.org";
        };
        "darwin@1.0.0" = {
          source = "npm";
          local = false;
          os = [ "darwin" ];
          cpu = null;
          registry = "registry.npmjs.org";
        };
        "arm@1.0.0" = {
          source = "npm";
          local = false;
          os = null;
          cpu = [ "arm64" ];
          registry = "registry.npmjs.org";
        };
        workspace = {
          source = "workspace";
          local = true;
          os = null;
          cpu = null;
          registry = null;
        };
      };
    };
  };

  normalized = api.normalizeBunNix canonical;
  filtered = api.filterPackagesForHost {
    bunNix = normalized;
    system = "x86_64-linux";
  };
  groups = api.groupResolutionsByConsumerSet normalized.metadata.productionDependencyClosures;
  cacheEntryCreator = pkgs.writeShellScriptBin "bun2nix" "exit 0";
  shard = api.mkCacheShard {
    inherit pkgs;
    name = "admin";
    sources = filtered;
    bun2nix = cacheEntryCreator;
    metadata = canonical.metadata.packages;
  };
  workspaceCaches = api.mkWorkspaceCaches {
    inherit pkgs;
    dependencyClosures = normalized.metadata.productionDependencyClosures;
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
    builtins.deepSeq (api.validateBunNix {
      packages = { };
      metadata = {
        lockfileVersion = 3;
        workspacePackages = [ ];
        productionDependencyClosures = {
          admin = [ "missing@1.0.0" ];
        };
        checkDependencyClosures = {
          admin = [ "missing@1.0.0" ];
        };
        developmentDependencyClosures = {
          admin = [ "missing@1.0.0" ];
        };
        packages = { };
      };
    }) true
  );
  invalidWorkspacePackages = builtins.tryEval (
    builtins.deepSeq (api.validateBunNix (
      canonical
      // {
        metadata = canonical.metadata // {
          workspacePackages = [ ];
        };
      }
    )) true
  );
in
assert normalized.metadata.workspacePackages == [ "workspace" ];
assert (api.normalizeBunNix { legacy = source "legacy"; }).metadata.workspacePackages == [ ];
assert
  builtins.attrNames filtered == [
    "common@1.0.0"
    "linux@1.0.0"
  ];
assert !(builtins.hasAttr "workspace" filtered);
assert groups."[\"admin\",\"worker\"]".resolutions == [ "common@1.0.0" ];
assert groups."[\"admin\"]".resolutions == [ "linux@1.0.0" ];
assert shard.dontFixup;
assert builtins.match ".*patchShebangs.*" shard.buildPhase != null;
assert builtins.match ".*cache-entry --out.*" shard.buildPhase != null;
assert pkgs.lib.hasInfix "registry_argument=(--registry \"$registry\")" shard.buildPhase;
assert pkgs.lib.hasInfix "common@1.0.0\t" shard.manifestText;
assert pkgs.lib.hasInfix "npm.private.example" shard.manifestText;
assert builtins.match ".*readlink -f.*" shard.buildPhase != null;
assert builtins.hasAttr "admin" workspaceCaches;
assert builtins.hasAttr "worker" workspaceCaches;
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
assert !invalidWorkspacePackages.success;
"bun2nix-nix-eval-tests"
