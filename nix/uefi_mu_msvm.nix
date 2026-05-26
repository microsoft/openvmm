{ system, stdenv, fetchzip, targetArch ? null }:

let
  # Allow explicit override of architecture, otherwise derive from host system
  # X64 uses VS2022 toolchain, AARCH64 uses CLANGPDB
  archToolchain = if targetArch == "x86_64" then "X64-VS2022"
         else if targetArch == "aarch64" then "AARCH64-CLANGPDB"
         else if system == "aarch64-linux" then "AARCH64-CLANGPDB"
         else "X64-VS2022";
  hash = {
    "AARCH64-CLANGPDB" = "sha256-9LkUNHeK3KLoxKIe5kl3uX2BwNlmvM0qJF2aIgwef08=";
    "X64-VS2022" = "sha256-51f/LWRfx5pg0ZjwI1FUxvqaXPO0F1A9dalFvtWDIv4=";
  }.${archToolchain};

in stdenv.mkDerivation {
  pname = "uefi-mu-msvm-${archToolchain}";
  version = "26.0.6";

  src = fetchzip {
    url =
      "https://github.com/microsoft/mu_msvm/releases/download/v26.0.6/RELEASE-${archToolchain}-artifacts.tar.gz";
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
