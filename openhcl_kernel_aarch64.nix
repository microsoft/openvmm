{ stdenv, fetchzip }:

# Wrapper to build aarch64 kernel regardless of host system
(import ./openhcl_kernel.nix {
  system = "aarch64-linux";
  inherit stdenv fetchzip;
  is_dev = false;
  is_cvm = false;
})
