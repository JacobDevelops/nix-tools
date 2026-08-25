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
        fileset = fileset.unions (
          [
            cargoFiles
            productionFiles
          ]
          ++ extraFiles
        );
      };
      check = mkSource {
        inherit root;
        fileset = fileset.unions (
          [
            cargoFiles
            checkFiles
          ]
          ++ extraFiles
        );
      };
    };

  stubMissingWorkspaceMembers =
    {
      root,
      memberPaths,
      includedMembers,
    }:
    lib.concatMapStringsSep "\n" (
      member:
      let
        relativeMemberPath = memberPaths.${member};
        memberPath = lib.escapeShellArg relativeMemberPath;
        manifest = builtins.fromTOML (builtins.readFile (root + "/${relativeMemberPath}/Cargo.toml"));
        targetPaths =
          kind: directory:
          map (target: target.path or "${directory}/${target.name}.rs") (manifest.${kind} or [ ]);
        buildPaths = lib.optional (builtins.isString (
          manifest.package.build or null
        )) manifest.package.build;
        stubPaths = lib.unique (
          [
            "src/lib.rs"
            "src/main.rs"
            ((manifest.lib or { }).path or "src/lib.rs")
          ]
          ++ buildPaths
          ++ targetPaths "bin" "src/bin"
          ++ targetPaths "test" "tests"
          ++ targetPaths "bench" "benches"
          ++ targetPaths "example" "examples"
        );
      in
      ''
        if [ ! -d ${memberPath}/src ]; then
          ${lib.concatMapStringsSep "\n" (path: ''
            mkdir -p ${memberPath}/$(dirname ${lib.escapeShellArg path})
            printf 'fn main() {}\n' > ${memberPath}/${lib.escapeShellArg path}
          '') stubPaths}
        fi
      ''
    ) (lib.filter (member: !(lib.elem member includedMembers)) (lib.attrNames memberPaths));

  validateRustCones =
    {
      root,
      memberPaths,
      cones,
    }:
    let
      members = lib.attrNames memberPaths;
      manifests = lib.mapAttrs (
        _: path: builtins.fromTOML (builtins.readFile (root + "/${path}/Cargo.toml"))
      ) memberPaths;
      membersByPackageName = lib.listToAttrs (
        map (member: lib.nameValuePair manifests.${member}.package.name member) members
      );
      dependencyTables =
        manifest:
        [
          (manifest.dependencies or { })
          (manifest."dev-dependencies" or { })
          (manifest."build-dependencies" or { })
        ]
        ++ lib.concatMap (target: [
          (target.dependencies or { })
          (target."dev-dependencies" or { })
          (target."build-dependencies" or { })
        ]) (lib.attrValues (manifest.target or { }));
      directDependencies = lib.mapAttrs (
        _: manifest:
        lib.unique (
          lib.concatMap (
            table:
            lib.concatMap (
              dependencyName:
              let
                specification = table.${dependencyName};
                packageName =
                  if builtins.isAttrs specification then specification.package or dependencyName else dependencyName;
                isWorkspaceDependency =
                  builtins.isAttrs specification && (specification ? path || (specification.workspace or false));
              in
              lib.optional (
                isWorkspaceDependency && builtins.hasAttr packageName membersByPackageName
              ) membersByPackageName.${packageName}
            ) (lib.attrNames table)
          ) (dependencyTables manifest)
        )
      ) manifests;
      closureFor =
        member:
        let
          visit =
            seen: pending:
            if pending == [ ] then
              seen
            else
              let
                current = lib.head pending;
                remaining = lib.tail pending;
                unseen = lib.filter (dependency: !(lib.elem dependency seen)) directDependencies.${current};
              in
              visit (seen ++ unseen) (remaining ++ unseen);
        in
        lib.sort builtins.lessThan (visit [ member ] [ member ]);
      valid = lib.all (
        member:
        let
          declared = lib.sort builtins.lessThan (lib.unique cones.${member});
          expected = closureFor member;
        in
        lib.assertMsg (declared == expected) (
          "nix framework: cone for '${member}' is ${builtins.toJSON declared}, expected ${builtins.toJSON expected} from Cargo path dependencies"
        )
      ) members;
    in
    assert lib.assertMsg (
      lib.sort builtins.lessThan (lib.attrNames cones) == lib.sort builtins.lessThan members
    ) "nix framework: cones and memberPaths must name the same workspace members";
    valid;

  # `memberPaths` maps package names to paths relative to `root`; no particular
  # workspace directory is assumed. Every cone retains every manifest so Cargo
  # can load the workspace, while `preConfigure` stubs members outside its cone.
  mkRustConeSources =
    {
      root,
      workspaceFiles,
      memberPaths,
      cones,
      memberSources,
      extraCompileInputs ? { },
    }:
    let
      allManifests = fileset.unions (
        [ workspaceFiles ] ++ lib.mapAttrsToList (name: path: root + "/${path}/Cargo.toml") memberPaths
      );
      sourceFor =
        member: kind:
        let
          cone = cones.${member};
          memberFiles = map (name: memberSources.${name}.${kind}) cone;
          compileInputs = lib.concatMap (
            name:
            let
              inputs = extraCompileInputs.${name} or [ ];
            in
            if builtins.isAttrs inputs then inputs.${kind} or [ ] else inputs
          ) cone;
        in
        mkSource {
          inherit root;
          fileset = fileset.unions ([ allManifests ] ++ memberFiles ++ compileInputs);
        };
    in
    lib.mapAttrs (member: _: {
      sources = {
        production = sourceFor member "production";
        check = sourceFor member "check";
      };
      preConfigure = stubMissingWorkspaceMembers {
        inherit root memberPaths;
        includedMembers = cones.${member};
      };
    }) cones;

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
      exposePackage ? true,
      exposeApp ? exposePackage,
      defaultPackage ? true,
      defaultApp ? true,
    }:
    let
      vendorArgs = lib.optionalAttrs (cargoVendorDir != null) { inherit cargoVendorDir; };
      common =
        src:
        commonArgs
        // vendorArgs
        // {
          pname = name;
          inherit
            version
            src
            nativeBuildInputs
            buildInputs
            ;
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
      packages =
        lib.optionalAttrs exposePackage {
          ${packageName} = package;
        }
        // lib.optionalAttrs (exposePackage && defaultPackage) { default = package; };
      checks = {
        inherit fmt clippy test;
      };
      apps =
        lib.optionalAttrs exposeApp {
          ${packageName} = app;
        }
        // lib.optionalAttrs (exposeApp && defaultApp) { default = app; };
    };

  mergeTargets =
    targetSets:
    let
      merge =
        field:
        lib.foldl' (
          merged: targets:
          lib.foldl' (
            result: name:
            if result ? ${name} then
              throw "nix framework: duplicate ${field} target '${name}'"
            else
              result // { ${name} = targets.${field}.${name}; }
          ) merged (lib.attrNames (targets.${field} or { }))
        ) { } targetSets;
    in
    {
      packages = merge "packages";
      checks = merge "checks";
      apps = merge "apps";
      devShells = merge "devShells";
    };

  mkRustPackageSet =
    {
      packageConfigs,
      defaultPackageName ? null,
      defaultAppName ? defaultPackageName,
    }:
    let
      targetsFor =
        config:
        if config ? cone then
          let
            commonArgs = config.commonArgs or { };
            cone = config.cone;
            preConfigure = lib.concatStringsSep "\n" (
              lib.filter (value: value != "") [
                cone.preConfigure
                (commonArgs.preConfigure or "")
              ]
            );
          in
          mkRustWorkspace (
            (builtins.removeAttrs config [
              "cone"
              "commonArgs"
              "sources"
            ])
            // {
              sources = cone.sources;
              commonArgs = (builtins.removeAttrs commonArgs [ "preConfigure" ]) // {
                inherit preConfigure;
              };
              defaultPackage = false;
              defaultApp = false;
            }
          )
        else
          mkRustWorkspace (
            config
            // {
              defaultPackage = false;
              defaultApp = false;
            }
          );
      targetsWithNamedChecks =
        config:
        let
          targets = targetsFor config;
        in
        targets
        // {
          checks = lib.mapAttrs' (
            checkName: check: lib.nameValuePair "${config.name}-${checkName}" check
          ) targets.checks;
        };
      merged = mergeTargets (lib.mapAttrsToList (_: targetsWithNamedChecks) packageConfigs);
    in
    merged
    // {
      packages =
        merged.packages
        // lib.optionalAttrs (defaultPackageName != null) {
          default = merged.packages.${defaultPackageName};
        };
      apps =
        merged.apps
        // lib.optionalAttrs (defaultAppName != null) {
          default = merged.apps.${defaultAppName};
        };
    };
in
{
  inherit
    mergeTargets
    mkApp
    mkRustConeSources
    mkRustPackageSet
    mkRustSources
    mkRustWorkspace
    stubMissingWorkspaceMembers
    validateRustCones
    ;
}
