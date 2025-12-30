{ stdenv, fetchzip }:

# Wrapper to build aarch64 UEFI firmware regardless of host system
(import ./uefi_mu_msvm.nix {
  system = "aarch64-linux";
  inherit stdenv fetchzip;
})
