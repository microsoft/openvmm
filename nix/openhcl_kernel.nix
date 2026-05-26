{ system, stdenv, fetchzip, targetArch ? null, is_dev ? false, is_cvm ? false }:

let
  version = if is_dev then "6.18.0.3" else "6.18.0.3";
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
        x64 = "sha256-bE7exmjCaR+g0pFKmQCpFgMS/7eqLCmWj4UN04qlCwQ=";
        arm64 = "sha256-FFC4826tOleHvRouF+KwquFPHQKT3hanUfXbWtWFO50=";
      };
      cvm = {
        x64 = "sha256-WtTHIHec4rmQPFCKfPzAdMDJkN0/WimV/l77ZX49KdU=";
        arm64 = throw "openhcl-kernel: cvm arm64 variant not available";
      };
    };
    hcl-dev = {
      std = {
        x64 = "sha256-l41pBU22R2NjdRpLT1JSwEj3ROf8Stqp6N5Rw/mi90E=";
        arm64 = "sha256-taGhLFIiYwmqeTk5zOUDkHafdyW7Nc7w95EMIuJaEy8=";
      };
      cvm = {
        x64 = "sha256-Ohjbjz1wjKjyKL9vCBejClNGYCS/0Ir7XtijKRJtvc0=";
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
