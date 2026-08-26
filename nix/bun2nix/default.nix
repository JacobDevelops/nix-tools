{ lib }:
let
  isStringList = value: builtins.isList value && lib.all builtins.isString value;

  isStringListOrNull = value: value == null || isStringList value;

  isPackageSource = value: builtins.isPath value || builtins.isString value || lib.isDerivation value;

  isPackageRow =
    row:
    builtins.isList row
    && builtins.length row == 5
    && builtins.isString (builtins.elemAt row 0)
    && isStringListOrNull (builtins.elemAt row 2)
    && isStringListOrNull (builtins.elemAt row 3)
    && ((builtins.elemAt row 4) == null || builtins.isString (builtins.elemAt row 4));

  indicesValid =
    upper: indices:
    builtins.isList indices
    && indices != [ ]
    && lib.all (index: builtins.isInt index && index >= 0 && index < upper) indices;

  unique =
    values:
    builtins.length values == builtins.length (
      builtins.attrNames (
        builtins.listToAttrs (
          map (value: {
            name = builtins.toString value;
            value = null;
          }) values
        )
      )
    );

  isGroupRow =
    workspaceCount: packageCount: row:
    builtins.isList row
    && builtins.length row == 2
    && indicesValid workspaceCount (builtins.elemAt row 0)
    && indicesValid packageCount (builtins.elemAt row 1)
    && unique (builtins.elemAt row 0)
    && unique (builtins.elemAt row 1);

  decodePackageRow = row: {
    resolution = builtins.elemAt row 0;
    source = builtins.elemAt row 1;
    os = builtins.elemAt row 2;
    cpu = builtins.elemAt row 3;
    registry = builtins.elemAt row 4;
  };

  platformForSystem =
    system:
    {
      x86_64-linux = {
        os = "linux";
        cpu = "x64";
      };
      i686-linux = {
        os = "linux";
        cpu = "ia32";
      };
      aarch64-linux = {
        os = "linux";
        cpu = "arm64";
      };
      armv7l-linux = {
        os = "linux";
        cpu = "arm";
      };
      powerpc64le-linux = {
        os = "linux";
        cpu = "ppc64le";
      };
      riscv64-linux = {
        os = "linux";
        cpu = "riscv64";
      };
      s390x-linux = {
        os = "linux";
        cpu = "s390x";
      };
      x86_64-darwin = {
        os = "darwin";
        cpu = "x64";
      };
      aarch64-darwin = {
        os = "darwin";
        cpu = "arm64";
      };
    }
    .${system} or (throw "bun2nix: unsupported host system '${system}'");

  matchesPlatform =
    restrictions: host:
    let
      allowed = lib.filter (value: !(lib.hasPrefix "!" value)) restrictions;
    in
    !(lib.elem "!${host}" restrictions) && (allowed == [ ] || lib.elem host allowed);

  packageMatchesHost =
    metadata: platform:
    (metadata.os == null || matchesPlatform metadata.os platform.os)
    && (metadata.cpu == null || matchesPlatform metadata.cpu platform.cpu);

  shardKey = consumers: builtins.toJSON consumers;

  cacheNameForShard = key: "bun-cache-${builtins.substring 0 16 (builtins.hashString "sha256" key)}";

  mkBunAsNode =
    pkgs:
    pkgs.runCommand "bun-as-node" { } ''
      mkdir -p "$out/bin"
      for alias in node npm npx; do
        ln -s ${lib.getExe pkgs.bun} "$out/bin/$alias"
      done
    '';

  mkBun2nixNoOp =
    pkgs:
    pkgs.runCommand "bun2nix-no-op" { } ''
      mkdir -p "$out/bin"
      printf '%s\n' '#!${pkgs.runtimeShell}' 'exit 0' > "$out/bin/bun2nix"
      chmod +x "$out/bin/bun2nix"
    '';
in
rec {
  validateBunNix =
    bunNix:
    let
      groupNames = [
        "production"
        "check"
        "development"
      ];
      groupValid =
        name:
        bunNix.groups ? ${name}
        && builtins.isList bunNix.groups.${name}
        && (
          let
            rows = bunNix.groups.${name};
            consumerKeys = map (row: builtins.toJSON (lib.sort builtins.lessThan (builtins.elemAt row 0))) rows;
            packageIndices = lib.concatMap (row: builtins.elemAt row 1) rows;
          in
          lib.all (isGroupRow (builtins.length bunNix.workspaces) (builtins.length bunNix.packages)) rows
          && unique consumerKeys
          && unique packageIndices
        );
    in
    assert lib.assertMsg (builtins.isAttrs bunNix) "bun2nix: bun.nix must evaluate to an attribute set";
    assert lib.assertMsg (
      bunNix ? format && bunNix.format == "bun2nix"
    ) "bun2nix: bun.nix format must be 'bun2nix'";
    assert lib.assertMsg (
      bunNix ? workspaces && isStringList bunNix.workspaces
    ) "bun2nix: bun.nix workspaces must be a list of strings";
    assert lib.assertMsg (unique bunNix.workspaces) "bun2nix: bun.nix workspace names must be unique";
    assert lib.assertMsg (
      bunNix ? packages && builtins.isList bunNix.packages
    ) "bun2nix: bun.nix packages must be a list";
    assert lib.assertMsg (lib.all isPackageRow bunNix.packages)
      "bun2nix: bun.nix packages contains an invalid compact package row";
    assert lib.assertMsg (unique (
      map (row: builtins.elemAt row 0) bunNix.packages
    )) "bun2nix: bun.nix package resolutions must be unique";
    assert lib.assertMsg (
      bunNix ? groups && builtins.isAttrs bunNix.groups
    ) "bun2nix: bun.nix groups must be an attribute set";
    assert lib.assertMsg (lib.all groupValid groupNames)
      "bun2nix: bun.nix groups contain an invalid compact group row or out-of-bounds index";
    bunNix;

  mkCacheShard =
    {
      pkgs,
      name,
      packages,
      bun2nix,
    }:
    let
      manifestText = lib.concatMapStrings (
        package:
        let
          source =
            assert lib.assertMsg (isPackageSource package.source)
              "bun2nix: selected package '${package.resolution}' has an invalid source";
            package.source;
        in
        "${package.resolution}\t${source}\t${if package.registry == null then "" else package.registry}\n"
      ) packages;
      manifest = pkgs.writeText "${name}-manifest" manifestText;
      bunAsNode = mkBunAsNode pkgs;
    in
    pkgs.stdenvNoCC.mkDerivation {
      inherit name;
      nativeBuildInputs = [
        pkgs.libarchive
        bun2nix
        bunAsNode
      ];
      dontUnpack = true;
      dontFixup = true;
      passthru = { inherit manifestText; };
      buildPhase = ''
        runHook preBuild

        mkdir -p "$out/share/bun-packages" "$out/share/bun-cache"

        extract_one() {
          local resolution="''${1%%$'\t'*}" remainder="''${1#*$'\t'}"
          local path="''${remainder%%$'\t'*}"
          local destination="$out/share/bun-packages/$resolution"
          mkdir -p "$destination"
          if [[ -d "$path" ]]; then
            cp -r "$path"/. "$destination"
          else
            bsdtar --extract --file "$path" --directory "$destination" \
              --strip-components=1 --no-same-owner --no-same-permissions
          fi
          chmod -R u+rwx "$destination"
        }
        export -f extract_one
        if [[ -s ${manifest} ]]; then
          xargs -d '\n' -n 1 -P "$NIX_BUILD_CORES" bash -c 'extract_one "$1"' _ < ${manifest}
        fi

        patchShebangs "$out/share/bun-packages"

        cache_entry() {
          local resolution="''${1%%$'\t'*}" remainder="''${1#*$'\t'}"
          local registry="''${remainder#*$'\t'}" registry_argument=()
          if [[ -n "$registry" ]]; then
            registry_argument=(--registry "$registry")
          fi
          bun2nix cache-entry --out "$out/share/bun-cache" \
            --name "$resolution" --package "$out/share/bun-packages/$resolution" "''${registry_argument[@]}"
        }
        export -f cache_entry
        if [[ -s ${manifest} ]]; then
          xargs -d '\n' -n 1 -P "$NIX_BUILD_CORES" bash -c 'cache_entry "$1"' _ < ${manifest}
        fi

        find "$out/share/bun-cache" -type l -print0 | while IFS= read -r -d $'\0' link; do
          ln -sfn "$(readlink -f "$link")" "$link"
        done

        runHook postBuild
      '';
      dontInstall = true;
    };

  mkCacheShards =
    {
      pkgs,
      bunNix,
      bun2nix,
      groupRows,
      system ? pkgs.stdenv.hostPlatform.system,
    }:
    let
      platform = platformForSystem system;
      packages = map decodePackageRow bunNix.packages;
      decodeGroup =
        row:
        let
          consumers = map (index: builtins.elemAt bunNix.workspaces index) (builtins.elemAt row 0);
          groupPackages = map (index: builtins.elemAt packages index) (builtins.elemAt row 1);
          cachePackages = lib.filter (package: packageMatchesHost package platform) groupPackages;
          key = shardKey consumers;
        in
        {
          name = key;
          value = {
            inherit consumers;
            resolutions = map (package: package.resolution) groupPackages;
            path = mkCacheShard {
              inherit pkgs bun2nix;
              name = cacheNameForShard key;
              packages = cachePackages;
            };
          };
        };
    in
    builtins.listToAttrs (map decodeGroup groupRows);

  mkWorkspaceCaches =
    {
      pkgs,
      workspaces,
      shards,
    }:
    lib.genAttrs workspaces (
      workspace:
      pkgs.symlinkJoin {
        name = "bun-cache-${workspace}";
        paths = lib.mapAttrsToList (_: shard: shard.path) (
          lib.filterAttrs (_: shard: lib.elem workspace shard.consumers) shards
        );
      }
    );

  mkSourceCone =
    {
      root,
      paths,
    }:
    lib.fileset.toSource {
      inherit root;
      fileset = lib.fileset.unions paths;
    };

  mkOfflineBunWorkspace =
    {
      pkgs,
      name,
      src,
      bunDeps,
      bun ? pkgs.bun,
      workspaceName ? name,
      installRoot ? ".",
      workspaceRoot ? ".",
      installFlags ? [
        "--frozen-lockfile"
        "--offline"
        "--linker=isolated"
        "--filter"
        workspaceName
      ],
      production ? false,
      lifecycle ? "ignore",
      lifecyclePhase ? null,
      build ? null,
      test ? null,
      bundle ? null,
      nativeBuildInputs ? [ ],
      installPhase ? null,
    }:
    assert lib.assertMsg (lib.elem lifecycle [
      "ignore"
      "run"
    ]) "bun2nix: lifecycle must be 'ignore' or 'run'";
    let
      bun2nixNoOp = mkBun2nixNoOp pkgs;
      installArguments = lib.concatMapStringsSep " " lib.escapeShellArg (
        installFlags
        ++ lib.optional production "--production"
        ++ lib.optional (lifecycle == "ignore") "--ignore-scripts"
      );
    in
    pkgs.stdenvNoCC.mkDerivation {
      inherit name src;
      nativeBuildInputs = [ bun ] ++ nativeBuildInputs;
      dontConfigure = true;
      doCheck = test != null;
      buildPhase = ''
        runHook preBuild

        chmod -R u+w .
        export BUN_INSTALL_CACHE_DIR="$TMPDIR/bun-cache"
        export PATH=${bun2nixNoOp}/bin:"$PATH"
        mkdir -p "$BUN_INSTALL_CACHE_DIR"
        cp -a ${bunDeps}/share/bun-cache/. "$BUN_INSTALL_CACHE_DIR/"
        (
          cd ${lib.escapeShellArg installRoot}
          bun install ${installArguments}
        )
        (
          cd ${lib.escapeShellArg workspaceRoot}
          ${lib.optionalString (build != null) build}
          ${lib.optionalString (bundle != null) bundle}
          ${lib.optionalString (lifecyclePhase != null) lifecyclePhase}
        )

        runHook postBuild
      '';
      checkPhase = lib.optionalString (test != null) ''
        runHook preCheck

        (
          cd ${lib.escapeShellArg workspaceRoot}
          ${test}
        )

        runHook postCheck
      '';
      installPhase =
        if installPhase != null then
          installPhase
        else
          ''
            runHook preInstall
            mkdir -p "$out"
            cp -r ./. "$out/"
            runHook postInstall
          '';
    };

  mkBunWorkspaceOutputs =
    {
      pkgs,
      bunNix,
      bun2nix,
      bun ? pkgs.bun,
      workspaces,
      system ? pkgs.stdenv.hostPlatform.system,
    }:
    let
      caches = mkBunCaches {
        inherit
          pkgs
          bunNix
          bun2nix
          system
          ;
      };
      mkWorkspace =
        workspace: config: dependencyKind: extra:
        mkOfflineBunWorkspace (
          {
            inherit pkgs bun;
            name = config.name or "bun-${workspace}";
            src = config.src;
            bunDeps = caches.${dependencyKind}.${workspace};
            workspaceName = config.workspaceName or workspace;
            installRoot = config.installRoot or ".";
            workspaceRoot = config.workspaceRoot or ".";
            installFlags =
              config.installFlags or [
                "--frozen-lockfile"
                "--offline"
                "--linker=isolated"
                "--filter"
                (config.workspaceName or workspace)
              ];
            production = extra.production or false;
            lifecycle = config.lifecycle or "ignore";
            lifecyclePhase = config.lifecyclePhase or null;
            build = config.build or null;
            test = config.test or null;
            bundle = config.bundle or null;
            nativeBuildInputs = config.nativeBuildInputs or [ ];
            installPhase = config.installPhase or null;
          }
          // extra
        );
      packages = lib.mapAttrs (
        workspace: config:
        mkWorkspace workspace config "productionWorkspaceCaches" {
          src = config.productionSrc or config.src;
          test = null;
          production = true;
        }
      ) workspaces;
      checks = lib.concatMapAttrs (
        workspace: config:
        lib.optionalAttrs ((config.test or null) != null) {
          "${workspace}-test" = mkWorkspace workspace config "checkWorkspaceCaches" {
            name = "${config.name or "bun-${workspace}"}-test";
            src = config.checkSrc or config.src;
            build = null;
            bundle = null;
            installPhase = "touch $out";
          };
        }
      ) workspaces;
      apps = lib.mapAttrs (
        workspace: config:
        let
          runner = pkgs.writeShellApplication {
            name = "${workspace}-run";
            runtimeInputs = [ pkgs.bun ] ++ (config.runtimeInputs or [ ]);
            text = ''
              cd ${lib.escapeShellArg "${packages.${workspace}}/${config.workspaceRoot or "."}"}
              exec ${config.run} "$@"
            '';
          };
        in
        {
          type = "app";
          program = "${runner}/bin/${workspace}-run";
        }
      ) (lib.filterAttrs (_: config: (config.run or null) != null) workspaces);
      devShells = lib.mapAttrs (
        _: config:
        pkgs.mkShell {
          packages = [ bun ] ++ (config.devShellInputs or [ ]);
          shellHook = config.shellHook or "";
        }
      ) workspaces;
    in
    {
      inherit
        packages
        checks
        apps
        devShells
        ;
      inherit (caches)
        productionWorkspaceCaches
        checkWorkspaceCaches
        developmentWorkspaceCaches
        ;
    };

  mkBunCaches =
    {
      pkgs,
      bunNix,
      bun2nix,
      system ? pkgs.stdenv.hostPlatform.system,
    }:
    let
      plan = validateBunNix bunNix;
      mkCaches =
        groupRows:
        let
          shards = mkCacheShards {
            inherit
              pkgs
              bun2nix
              system
              ;
            bunNix = plan;
            inherit groupRows;
          };
        in
        {
          inherit shards;
          workspaceCaches = mkWorkspaceCaches {
            inherit pkgs shards;
            workspaces = plan.workspaces;
          };
        };
      production = mkCaches plan.groups.production;
      check = mkCaches plan.groups.check;
      development = mkCaches plan.groups.development;
    in
    {
      inherit
        production
        check
        development
        ;
      productionWorkspaceCaches = production.workspaceCaches;
      checkWorkspaceCaches = check.workspaceCaches;
      developmentWorkspaceCaches = development.workspaceCaches;
    };
}
