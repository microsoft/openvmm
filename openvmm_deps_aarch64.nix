{ stdenv, fetchzip }:

# Wrapper to fetch aarch64 openvmm_deps regardless of host system
let
  hash = "sha256-yLGLoQrzA07jrG4G1HMb2P3fcmnGS3KF5H/4AtzDO4w=";
in stdenv.mkDerivation {
  pname = "openvmm-deps";
  version = "0.1.0-20250403.3";

  src = fetchzip {
    url =
      "https://github.com/microsoft/openvmm-deps/releases/download/0.1.0-20250403.3/openvmm-deps.aarch64.0.1.0-20250403.3.tar.bz2";
    stripRoot = false;
    inherit hash;
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp -r * $out/
    runHook postInstall
  '';
}
