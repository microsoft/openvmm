{ system, stdenv, fetchzip, gnutar, gzip, targetArch ? null }:

let
  # Allow explicit override of architecture, otherwise derive from host system
  arch = if targetArch != null then targetArch
         else if system == "aarch64-linux" then "aarch64"
         else "x86_64";
  hash = {
    "aarch64" = "sha256-WcY2nAO73/sF7BYTlMuNCLPqMq8VkuB92AnNxXijwck=";
    "x86_64" = "sha256-DPU5raOWwrLQje6KUKq9HBqwbXdQ0CVzdch8yI4VVY0=";
  }.${arch};

in stdenv.mkDerivation {
  pname = "openvmm-deps-${arch}";
  version = "0.3.0-155";

  src = fetchzip {
    url =
      "https://github.com/microsoft/openvmm-deps/releases/download/0.3.0-155/openvmm-deps.${arch}.0.3.0-155.tar.gz";
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
