{ stdenv, fetchzip }:

# Wrapper to build x64 kernel regardless of host system
(import ./openhcl_kernel.nix {
  system = "x86_64-linux";
  inherit stdenv fetchzip;
  is_dev = false;
  is_cvm = false;
})
