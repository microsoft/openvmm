{ stdenv, fetchzip }:

# Wrapper to build x64 UEFI firmware regardless of host system
(import ./uefi_mu_msvm.nix {
  system = "x86_64-linux";
  inherit stdenv fetchzip;
})
