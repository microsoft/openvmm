{ system, stdenv, fetchzip, targetArch ? null, is_dev ? false, is_cvm ? false }:

let
  version = if is_dev then "6.18.0.7" else "6.18.0.7";
  # Allow explicit override of architecture, otherwise derive from host system
  # Note: targetArch uses "x86_64"/"aarch64", but URLs use "x64"/"arm64"
  arch = if targetArch == "x86_64" then "x64"
         else if targetArch == "aarch64" then "arm64"
         else if system == "aarch64-linux" then "arm64"
         else "x64";
  branch = if is_dev then "hcl-dev" else "hcl-main";
  build_type = if is_cvm then "cvm" else "std";
  # See https://github.com/microsoft/OHCL-Linux-Kernel/releases
  url =
    "https://github.com/microsoft/OHCL-Linux-Kernel/releases/download/rolling-lts/${branch}/${version}/Microsoft.OHCL.Kernel${
      if is_dev then ".Dev" else ""
    }.${version}-${if is_cvm then "cvm-" else ""}${arch}.tar.gz";
  hashes = {
    hcl-main = {
      std = {
        x64 = "sha256-OPgJGihkrdJtoZhOdRRct55exP2HY8qepUuQqnmZdKQ=";
        arm64 = "sha256-s49kGBGhDY3muXA6FjjztoNXjfzOdtEXnHR4LLnqY+c=";
      };
      cvm = {
        x64 = "sha256-/T7D4lcl30q3GNsbZ1XdGlLxfdrlzUeJ8k3vA40xa1k=";
        arm64 = throw "openhcl-kernel: cvm arm64 variant not available";
      };
    };
    hcl-dev = {
      std = {
        x64 = "sha256-PUqcTMu52LCQzemRJLaf2xcpyTuA0EHIzXHqzmSq+2A=";
        arm64 = "sha256-B/rTpIr9ZbBbxhCt9vdoz5K01rN0bZ9E6/P4q2nrxNo=";
      };
      cvm = {
        x64 = "sha256-L+VF1NdQhzTU1T8K8NPP66XIJeKrj15oEw0eQjpzRpg=";
        arm64 = throw "openhcl-kernel: dev cvm arm64 variant not available";
      };
    };
  };
  hash = hashes.${branch}.${build_type}.${arch};

  # Build a descriptive pname that includes variant info
  variant = "${if is_cvm then "-cvm" else ""}${if is_dev then "-dev" else ""}";

in stdenv.mkDerivation {
  pname = "openhcl-kernel-${arch}${variant}";
  inherit version;
  src = fetchzip {
    inherit url;
    stripRoot = false;
    inherit hash;
  };

  dontConfigure = true;
  dontBuild = true;

  installPhase = ''
    runHook preInstall
    mkdir -p $out/modules
    # x64 uses vmlinux, arm64 uses Image
    if [ -f vmlinux ]; then
      cp vmlinux* $out/
    fi
    if [ -f Image ]; then
      cp Image $out/
    fi
    cp -r modules/* $out/modules/
    cp kernel_build_metadata.json $out/
    if [ -d tools ]; then
      cp -r tools $out/
    fi
    runHook postInstall
  '';
}
