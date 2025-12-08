{ system, stdenv, fetchzip, }:

let

  arch = if system == "aarch64-linux" then "AARCH64" else "x64";
  hash = if system == "aarch64-linux" then
    "sha256-ybBnZZssTXrOxqH6NudmYljn92ejCjrazBIv/GNQyn4="
  else
    "sha256-ybBnZZssTXrOxqH6NudmYljn92ejCjrazBIv/GNQyn4=";

in stdenv.mkDerivation {
  pname = "lxutil";
  version = "10.0.26100.1-240331-1435.ge-release";

  src = fetchzip {
    url =
      "https://github.com/microsoft/openvmm-deps/releases/download/Microsoft.WSL.LxUtil.10.0.26100.1-240331-1435.ge-release/Microsoft.WSL.LxUtil.${arch}.zip";
    inherit hash;
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    cp bin/lxutil.dll $out/bin/lxutil.dll
    runHook postInstall
  '';
}
