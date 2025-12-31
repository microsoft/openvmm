{ system, stdenv, fetchzip, }:

let

  arch = if system == "aarch64-linux" then "aarch64" else "x86_64";
  hash = if system == "aarch64-linux" then
    "sha256-yLGLoQrzA07jrG4G1HMb2P3fcmnGS3KF5H/4AtzDO4w="
  else
    "sha256-uDCEo4wbHya3KEYVgFHxr+/OOkzyMCUwhLNX7kppojQ=";

in stdenv.mkDerivation {
  pname = "openvmm-deps";
  version = "0.1.0-20250403.3";

  src = fetchzip {
    url =
      "https://github.com/microsoft/openvmm-deps/releases/download/0.1.0-20250403.3/openvmm-deps.${arch}.0.1.0-20250403.3.tar.bz2";
    stripRoot = false;
    inherit hash;
  };

  installPhase = ''
    runHook preInstall
    mkdir $out
    cp * $out
    runHook postInstall
  '';
}
