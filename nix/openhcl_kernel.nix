{ system, stdenv, fetchzip, targetArch ? null, is_dev ? false, is_cvm ? false }:

let
  version = if is_dev then "6.18.namjain.cdevtest" else "6.18.namjain.cdevtest";
  # Allow explicit override of architecture, otherwise derive from host system
  # Note: targetArch uses "x86_64"/"aarch64", but URLs use "x64"/"arm64"
  arch = if targetArch == "x86_64" then "x64"
         else if targetArch == "aarch64" then "arm64"
         else if system == "aarch64-linux" then "arm64"
         else "x64";
  # The custom 6.18.namjain.cdevtest kernel is published only under the
  # hcl-main release tag, so use it for the dev variant too.
  branch = "hcl-main";
  build_type = if is_cvm then "cvm" else "std";
  # See https://github.com/microsoft/OHCL-Linux-Kernel/releases
  # This release ships only ".Dev"-flavored assets, so always use that name.
  url =
    "https://github.com/microsoft/OHCL-Linux-Kernel/releases/download/rolling-lts/${branch}/${version}/Microsoft.OHCL.Kernel.Dev.${version}-${if is_cvm then "cvm-" else ""}${arch}.tar.gz";
  hashes = {
    hcl-main = {
      std = {
        x64 = "sha256-/Gcaq5O6Ubp2W0/9uENv87vxDLoCHS+ZA/P9iJ1KFfk=";
        arm64 = "sha256-+X8w7odWDfMN5hCv6/7PQcPk60zXLV6EEoox0LJW+xg=";
      };
      cvm = {
        x64 = "sha256-MrilwqhbzY8yXRGxIG4sdtZByL98xyrGjpme7/MA6Ow=";
        arm64 = throw "openhcl-kernel: cvm arm64 variant not available";
      };
    };
    hcl-dev = {
      std = {
        x64 = "sha256-/Gcaq5O6Ubp2W0/9uENv87vxDLoCHS+ZA/P9iJ1KFfk=";
        arm64 = "sha256-+X8w7odWDfMN5hCv6/7PQcPk60zXLV6EEoox0LJW+xg=";
      };
      cvm = {
        x64 = "sha256-MrilwqhbzY8yXRGxIG4sdtZByL98xyrGjpme7/MA6Ow=";
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
