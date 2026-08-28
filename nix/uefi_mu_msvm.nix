{ system, stdenv, fetchzip, targetArch ? null }:

let
  # Allow explicit override of architecture, otherwise derive from host system
  # X64 uses VS2022 toolchain, AARCH64 uses CLANGPDB
  archToolchain = if targetArch == "x86_64" then "X64-VS2022"
         else if targetArch == "aarch64" then "AARCH64-CLANGPDB"
         else if system == "aarch64-linux" then "AARCH64-CLANGPDB"
         else "X64-VS2022";
  hash = {
    "AARCH64-CLANGPDB" = "sha256-bsAFSiowkENDuXf/85vNLxxcbO8UeAYaR3jn44HuiKU=";
    "X64-VS2022" = "sha256-ZlAjWQ02/tzRmT4E+Y5xNtixaMM5j8KWjWUIpU/F/dw=";
  }.${archToolchain};

in stdenv.mkDerivation rec {
  pname = "uefi-mu-msvm-${archToolchain}";
  version = "26.0.25";

  src = fetchzip {
    url =
      "https://github.com/microsoft/mu_msvm/releases/download/v${version}/RELEASE-${archToolchain}-artifacts.tar.gz";
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
