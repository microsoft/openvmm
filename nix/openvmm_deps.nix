{ system, stdenv, fetchzip, gnutar, gzip, targetArch ? null }:

let
  # Allow explicit override of architecture, otherwise derive from host system
  arch = if targetArch != null then targetArch
         else if system == "aarch64-linux" then "aarch64"
         else "x86_64";
  hash = {
    "aarch64" = "sha256-4Q63EMsSgxLtU4c7l+dvGlxyVpk5ewKtTn+WgF7ScBM=";
    "x86_64" = "sha256-XYSAKiRGYqPghEa2cMijgf9k21AiDhkCxIbxE/OCk5o=";
  }.${arch};

in stdenv.mkDerivation {
  pname = "openvmm-deps-${arch}";
  version = "0.3.0-33";

  src = fetchzip {
    url =
      "https://github.com/microsoft/openvmm-deps/releases/download/0.3.0-33/openvmm-deps.${arch}.0.3.0-33.tar.gz";
    stripRoot = false;
    inherit hash;
  };

  nativeBuildInputs = [ gnutar gzip ];

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out

    # Copy all original files (including sysroot.tar.gz for flowey compatibility)
    cp -r * $out/

    # Also extract sysroot.tar.gz so that $out is a valid sysroot path
    # (lib/, include/, etc. at top level for the linker wrapper)
    tar -xzf sysroot.tar.gz -C $out

    runHook postInstall
  '';
}
