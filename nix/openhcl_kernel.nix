{ system, stdenv, fetchzip, targetArch ? null, is_dev ? false, is_cvm ? false }:

let
  version = if is_dev then "deprecated" else "7.3.4.1.test";
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
        x64 = "sha256-f49F75tyqnN5sTpIuXmbYqhdVVpmA9vu8pkJF0v/Mo4=";
        arm64 = "sha256-HuyIjM6LrcipmpcuB5dmwWs3ViIJ53/sjfY6Uf6Vnz0=";
      };
      cvm = {
        x64 = "sha256-XNA8F3/dsvqzrvPIhMwUvnbTiKRgFXZBsjG2AXGsEDU=";
        arm64 = throw "openhcl-kernel: cvm arm64 variant not available";
      };
    };
    hcl-dev = throw "openhcl-kernel: dev kernel has been deprecated";
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
