{ stdenv, fetchzip }:

# Wrapper to fetch x64 openvmm_deps regardless of host system
let
  hash = "sha256-uDCEo4wbHya3KEYVgFHxr+/OOkzyMCUwhLNX7kppojQ=";
in stdenv.mkDerivation {
  pname = "openvmm-deps";
  version = "0.1.0-20250403.3";

  src = fetchzip {
    url =
      "https://github.com/microsoft/openvmm-deps/releases/download/0.1.0-20250403.3/openvmm-deps.x86_64.0.1.0-20250403.3.tar.bz2";

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
