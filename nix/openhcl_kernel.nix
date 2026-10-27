{ system, stdenv, fetchzip, targetArch ? null, is_dev ? false, is_cvm ? false }:

let
  version = if is_dev then "deprecated" else "6.18.37.6-iommufd-cdev-v2";
  # Allow explicit override of architecture, otherwise derive from host system
  # Note: targetArch uses "x86_64"/"aarch64", but URLs use "x64"/"arm64"
  arch = if targetArch == "x86_64" then "x64"
         else if targetArch == "aarch64" then "arm64"
         else if system == "aarch64-linux" then "arm64"
         else "x64";
  branch = if is_dev then "hcl-dev" else "hcl-main";
  build_type = if is_cvm then "cvm" else "std";
  # See https://github.com/microsoft/OHCL-Linux-Kernel/releases
  # The release workflow uses .Dev assets because the tag contains "cdev".
  url =
    "https://github.com/microsoft/OHCL-Linux-Kernel/releases/download/rolling-lts/${branch}/${version}/Microsoft.OHCL.Kernel.Dev.${version}-${if is_cvm then "cvm-" else ""}${arch}.tar.gz";
  hashes = {
    hcl-main = {
      std = {
        x64 = "sha256-wRrmZho2T8+Rtlf5jC2HNQuUNWjr4N/u1UnBzHH0Mfg=";
        arm64 = "sha256-/22sN7fRSCHY2tctx0VvC0U1xALKIAK/L1MWM2WQao8=";
      };
      cvm = {
        x64 = "sha256-YmkdOZ9o/g6yq5ucDlk91tfDHddSKiyl4NNn4pFSw/s=";
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
