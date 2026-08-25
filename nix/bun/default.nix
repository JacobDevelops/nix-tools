{
  pkgs,
  lib ? pkgs.lib,
  version ? "1.4.0",
}:
let
  artifacts = {
    x86_64-linux = {
      name = "bun-linux-x64-baseline";
      hash = "sha256-Poy0vf7yJ/hzk33QiQj5gnshI5Q7dfbaMD7xgwiyDKw=";
    };
    aarch64-linux = {
      name = "bun-linux-aarch64";
      hash = "sha256-rIfaywTWWN3ELVH9DtPfrkuAGjrwi7DJYUeKPS1Zd04=";
    };
    x86_64-darwin = {
      name = "bun-darwin-x64";
      hash = "sha256-v1gYYeMeh12HwAF5/XkhESgghCzug9Ze9tdB9CSJOYA=";
    };
    aarch64-darwin = {
      name = "bun-darwin-aarch64";
      hash = "sha256-ggJttQZ3702q2fiM1bEh14l2gHOxmbd1K0dmS7QS7lQ=";
    };
  };
  artifact = artifacts.${pkgs.stdenv.hostPlatform.system} or (throw "unsupported Bun platform");
  source = pkgs.fetchzip {
    url = "https://github.com/oven-sh/bun/releases/download/bun-v${version}/${artifact.name}.zip";
    inherit (artifact) hash;
  };
in
pkgs.stdenvNoCC.mkDerivation {
  pname = "bun";
  inherit version;
  dontUnpack = true;
  nativeBuildInputs = lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.autoPatchelfHook ];
  buildInputs = [ pkgs.openssl ];
  installPhase = ''
    mkdir -p "$out/bin"
    install -m755 ${source}/bun "$out/bin/bun"
    ln -s bun "$out/bin/bunx"
  '';
  postFixup = lib.optionalString pkgs.stdenv.hostPlatform.isDarwin ''
    '${lib.getExe' pkgs.cctools "${pkgs.cctools.targetPrefix}install_name_tool"}' "$out/bin/bun" \
      -change /usr/lib/libicucore.A.dylib '${lib.getLib pkgs.darwin.ICU}/lib/libicucore.A.dylib'
    '${lib.getExe pkgs.rcodesign}' sign --code-signature-flags linker-signed "$out/bin/bun"
  '';
  meta = {
    description = "Bun JavaScript runtime, bundler, test runner, and package manager";
    homepage = "https://bun.com";
    license = [
      lib.licenses.mit
      lib.licenses.lgpl21Only
    ];
    mainProgram = "bun";
    platforms = builtins.attrNames artifacts;
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
  };
}
