{ system, stdenv, fetchzip, targetArch ? null }:

let
  # Allow explicit override of architecture, otherwise derive from host system
  # Both architectures use the legacy C core with CLANGPDB
  archToolchain = if targetArch == "x86_64" then "X64-CLANGPDB"
         else if targetArch == "aarch64" then "AARCH64-CLANGPDB"
         else if system == "aarch64-linux" then "AARCH64-CLANGPDB"
         else "X64-CLANGPDB";
  hash = {
    "AARCH64-CLANGPDB" = "sha256-dkRfShH2GW3k3tH8CBnYYpIq/Wi5B8cg2yp3F2wLhwM=";
    "X64-CLANGPDB" = "sha256-Ww1plif/mcGnC91Mk4WkkCYN3fEyYi35ocvxzcWMMoM=";
  }.${archToolchain};

in stdenv.mkDerivation rec {
  pname = "uefi-mu-msvm-${archToolchain}";
  version = "26.0.44";

  src = fetchzip {
    url =
      "https://github.com/microsoft/mu_msvm/releases/download/v${version}/firmware-RELEASE-${archToolchain}.tar.gz";
    stripRoot = false;
    inherit hash;
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir $out
    cp FV/MSVM.fd $out
    runHook postInstall
  '';
}
