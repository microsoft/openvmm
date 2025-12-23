{ system, stdenv, fetchzip, is_dev ? false, is_cvm ? false, }:

let
  version = if is_dev then "6.12.44.1" else "6.12.44.1";
  arch = if system == "aarch64-linux" then "arm64" else "x64";
  branch = if is_dev then "hcl-dev" else "hcl-main";
  build_type = if is_cvm then "cvm" else "std";
  # See https://github.com/microsoft/OHCL-Linux-Kernel/releases
  url =
    "https://github.com/microsoft/OHCL-Linux-Kernel/releases/download/rolling-lts/${branch}/${version}/Microsoft.OHCL.Kernel${
      if is_dev then ".Dev" else ""
    }.${version}-${if is_cvm then "cvm-" else ""}${arch}.tar.gz";
  hash = {
    hcl-main = {
      std = {
        x64 = "sha256-An1N76i1MPb+rrQ1nBpoiuxnNeD0E+VuwqXdkPzaZn0=";
        arm64 = "sha256-ENjd+Pd9sQ/f0Gvyq0iB+IG7c4p+AxwxoWu87pZSXYQ=";
      };
      cvm = { x64 = "sha256-pV/20epW9LYWzwA633MYxtaUCyMaLAWaaSEJyx+rniQ="; };
    };
    hcl-dev = {
      std = {
        x64 = "sha256-Ow9piuc2IDR4RPISKY5EAQ5ykjitem4CXS9974lvwPE=";
        arm64 = "";
      };
      cvm = {
        x64 = "sha256-IryjvoFDSghhVucKlIG9V0IzcVuf8m8Cmk5NhhWzTQM=";
      };
    };
  }.${branch}.${build_type}.${arch};

in stdenv.mkDerivation {
  pname = "openhcl-kernel";
  inherit version;
  src = fetchzip {
    inherit url;
    stripRoot = false;
    inherit hash;
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/build/native/bin/${arch}
    cp vmlinux* $out/build/native/bin/${arch}/
    cp kernel_build_metadata.json $out/build/native/bin/
    cp -r modules $out/build/native/bin/${arch}/
    runHook postInstall
  '';
}
