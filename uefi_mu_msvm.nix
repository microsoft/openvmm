{ system, stdenv, fetchzip, }:

let

  arch = if system == "aarch64-linux" then "AARCH64" else "x64";
  hash = if system == "aarch64-linux" then
    "sha256-WFMMf9LdCd0X6jwPVhYScmoXjpQdJnswFwHjMWvmZz8="
  else
    "sha256-wJeRZC6sd+tNSYHdyyN4Qj/sn5puT6R8eagFlHa6pP4=";

in stdenv.mkDerivation {
  pname = "openvmm-deps";
  version = "24.0.4";

  src = fetchzip {
    url =
      "https://github.com/microsoft/mu_msvm/releases/download/v24.0.4/RELEASE-${arch}-artifacts.zip";
    stripRoot = false;
    inherit hash;
  };

  installPhase = ''
    runHook preInstall
    mkdir $out
    cp FV/MSVM.fd $out
    runHook postInstall
  '';
}
