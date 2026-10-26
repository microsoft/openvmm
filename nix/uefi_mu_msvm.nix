{ system, stdenv, fetchzip, targetArch ? null }:

let
  # Allow explicit override of architecture, otherwise derive from host system
  # Both architectures use the legacy C core with CLANGPDB
  archToolchain = if targetArch == "x86_64" then "X64-CLANGPDB"
         else if targetArch == "aarch64" then "AARCH64-CLANGPDB"
         else if system == "aarch64-linux" then "AARCH64-CLANGPDB"
         else "X64-CLANGPDB";
  hash = {
    "AARCH64-CLANGPDB" = "sha256-S1HhZoj5wtTXACXGp1zR+JaNtvJO71R6DQ8Qa3dWt30=";
    "X64-CLANGPDB" = "sha256-NfGZyWsuvsVCbeiKAsofO7U++w88ynv4SotxzI2J7sI=";
  }.${archToolchain};

in stdenv.mkDerivation rec {
  pname = "uefi-mu-msvm-${archToolchain}";
  version = "26.0.42";

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
