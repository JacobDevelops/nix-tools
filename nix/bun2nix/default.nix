{ lib }:
let
  sourceKinds = [
    "npm"
    "workspace"
    "file"
    "folder"
    "link"
    "root"
    "tarball"
    "github"
    "git"
  ];

  isStringList = value: builtins.isList value && lib.all builtins.isString value;

  isStringListOrNull = value: value == null || isStringList value;

  defaultPackageMetadata = {
    source = "npm";
    local = false;
    os = null;
    cpu = null;
    registry = null;
  };

  normalizeLegacy = packages: {
    inherit packages;
    metadata = {
      lockfileVersion = 0;
      workspacePackages = [ ];
      dependencyClosures = { };
      packages = lib.genAttrs (builtins.attrNames packages) (_: defaultPackageMetadata);
    };
  };

  isPackageMetadata =
    metadata:
    builtins.isAttrs metadata
    && metadata ? source
    && builtins.isString metadata.source
    && lib.elem metadata.source sourceKinds
    && metadata ? local
    && builtins.isBool metadata.local
    && metadata ? os
    && isStringListOrNull metadata.os
    && metadata ? cpu
    && isStringListOrNull metadata.cpu
    && metadata ? registry
    && (metadata.registry == null || builtins.isString metadata.registry);

  isLocal =
    metadata:
    metadata.local
    || lib.elem metadata.source [
      "workspace"
      "file"
      "folder"
      "link"
      "root"
    ];

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
  # Canonical output carries package metadata. Legacy flat generated files are
  # treated as unrestricted remote packages so existing consumers still evaluate.
  normalizeBunNix =
    bunNix: validateBunNix (if bunNix ? packages then bunNix else normalizeLegacy bunNix);

  validateBunNix =
    bunNix:
    let
      packageNames = builtins.attrNames bunNix.packages;
      metadata = bunNix.metadata;
      metadataNames = builtins.attrNames metadata.packages;
      closureNames = builtins.attrNames metadata.dependencyClosures;
      closureResolutions = lib.concatLists (builtins.attrValues metadata.dependencyClosures);
      localResolutions = lib.filter (name: isLocal metadata.packages.${name}) packageNames;
    in
    assert lib.assertMsg (builtins.isAttrs bunNix) "bun2nix: bun.nix must evaluate to an attribute set";
    assert lib.assertMsg (
      bunNix ? packages && builtins.isAttrs bunNix.packages
    ) "bun2nix: bun.nix packages must be an attribute set";
    assert lib.assertMsg (
      bunNix ? metadata && builtins.isAttrs bunNix.metadata
    ) "bun2nix: canonical bun.nix requires metadata";
    assert lib.assertMsg (
      metadata ? lockfileVersion && builtins.isInt metadata.lockfileVersion
    ) "bun2nix: metadata.lockfileVersion must be an integer";
    assert lib.assertMsg (
      metadata ? workspacePackages && isStringList metadata.workspacePackages
    ) "bun2nix: metadata.workspacePackages must be a list of resolutions";
    assert lib.assertMsg (
      metadata ? dependencyClosures && builtins.isAttrs metadata.dependencyClosures
    ) "bun2nix: metadata.dependencyClosures must be an attribute set";
    assert lib.assertMsg (
      metadata ? packages && builtins.isAttrs metadata.packages
    ) "bun2nix: metadata.packages must be an attribute set";
    assert lib.assertMsg (lib.all (
      name: isStringList metadata.dependencyClosures.${name}
    ) closureNames) "bun2nix: every dependency closure must be a list of resolutions";
    assert lib.assertMsg (lib.all (
      name: isPackageMetadata metadata.packages.${name}
    ) metadataNames) "bun2nix: metadata.packages contains an invalid package entry";
    assert lib.assertMsg (
      lib.sort builtins.lessThan packageNames == lib.sort builtins.lessThan metadataNames
    ) "bun2nix: package sources and metadata must name the same resolutions";
    assert lib.assertMsg (lib.all (name: lib.elem name packageNames)
      metadata.workspacePackages
    ) "bun2nix: metadata.workspacePackages references an unknown resolution";
    assert lib.assertMsg (
      metadata.workspacePackages == localResolutions
    ) "bun2nix: metadata.workspacePackages must be the sorted local package resolutions";
    assert lib.assertMsg (lib.all (
      name: lib.elem name packageNames
    ) closureResolutions) "bun2nix: a dependency closure references an unknown resolution";
    bunNix;

  # Only metadata can remove a remote package for a host. Local/workspace
  # entries are excluded because Bun resolves them from the consumer source tree.
  filterPackagesForHost =
    {
      bunNix,
      system,
    }:
    let
      normalized = normalizeBunNix bunNix;
      platform = platformForSystem system;
    in
    lib.filterAttrs (
      resolution: _:
      let
        metadata = normalized.metadata.packages.${resolution};
      in
      !isLocal metadata && packageMatchesHost metadata platform
    ) normalized.packages;

  # Each key is the JSON representation of a sorted exact consumer set.
  groupResolutionsByConsumerSet =
    dependencyClosures:
    let
      workspaces = builtins.attrNames dependencyClosures;
      resolutions = lib.unique (lib.concatLists (builtins.attrValues dependencyClosures));
      addResolution =
        groups: resolution:
        let
          consumers = lib.filter (workspace: lib.elem resolution dependencyClosures.${workspace}) workspaces;
          key = shardKey consumers;
          previous =
            groups.${key} or {
              inherit consumers;
              resolutions = [ ];
            };
        in
        groups
        // {
          ${key} = previous // {
            resolutions = previous.resolutions ++ [ resolution ];
          };
        };
    in
    lib.foldl' addResolution { } resolutions;

  mkCacheShard =
    {
      pkgs,
      name,
      sources,
      bun2nix,
      metadata ? { },
    }:
    let
      manifestText = lib.concatStrings (
        lib.mapAttrsToList (
          resolution: path:
          let
            packageMetadata = metadata.${resolution} or { };
            registry = packageMetadata.registry or null;
          in
          "${resolution}\t${path}\t${if registry == null then "" else registry}\n"
        ) sources
      );
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
      system ? pkgs.stdenv.hostPlatform.system,
    }:
    let
      normalized = normalizeBunNix bunNix;
      cacheablePackages = filterPackagesForHost {
        bunNix = normalized;
        inherit system;
      };
      groups = groupResolutionsByConsumerSet normalized.metadata.dependencyClosures;
    in
    lib.mapAttrs (
      key: group:
      group
      // {
        path = mkCacheShard {
          inherit pkgs bun2nix;
          name = cacheNameForShard key;
          sources = lib.filterAttrs (resolution: _: lib.elem resolution group.resolutions) cacheablePackages;
          metadata = normalized.metadata.packages;
        };
      }
    ) groups;

  mkWorkspaceCaches =
    {
      pkgs,
      dependencyClosures,
      shards,
    }:
    lib.mapAttrs (
      workspace: _:
      pkgs.symlinkJoin {
        name = "bun-cache-${workspace}";
        paths = lib.mapAttrsToList (_: shard: shard.path) (
          lib.filterAttrs (_: shard: lib.elem workspace shard.consumers) shards
        );
      }
    ) dependencyClosures;

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
        installFlags ++ lib.optional (lifecycle == "ignore") "--ignore-scripts"
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
        workspace: config: extra:
        mkOfflineBunWorkspace (
          {
            inherit pkgs bun;
            name = config.name or "bun-${workspace}";
            src = config.src;
            bunDeps = caches.workspaceCaches.${workspace};
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
        mkWorkspace workspace config {
          src = config.productionSrc or config.src;
          test = null;
        }
      ) workspaces;
      checks = lib.concatMapAttrs (
        workspace: config:
        lib.optionalAttrs ((config.test or null) != null) {
          "${workspace}-test" = mkWorkspace workspace config {
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
    };

  mkBunCaches =
    {
      pkgs,
      bunNix,
      bun2nix,
      system ? pkgs.stdenv.hostPlatform.system,
    }:
    let
      normalized = normalizeBunNix bunNix;
      filteredPackages = filterPackagesForHost {
        bunNix = normalized;
        inherit system;
      };
      shards = mkCacheShards {
        inherit
          pkgs
          bun2nix
          system
          ;
        bunNix = normalized;
      };
    in
    {
      inherit
        normalized
        filteredPackages
        shards
        ;
      workspaceCaches = mkWorkspaceCaches {
        inherit pkgs shards;
        dependencyClosures = normalized.metadata.dependencyClosures;
      };
    };
}
