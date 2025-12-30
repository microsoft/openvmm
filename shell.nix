let
  rust_overlay = import (builtins.fetchTarball
    "https://github.com/oxalica/rust-overlay/archive/master.tar.gz");
  # pkgs = import <nixpkgs> { crossSystem =  { config = "aarch64-unknown-linux-gnu"; }; overlays = [ rust_overlay ]; };
  pkgs = import <nixpkgs> { overlays = [ rust_overlay ]; };

  mdbook = pkgs.callPackage ./mdbook.nix { };
  mdbook_admonish = pkgs.callPackage ./mdbook_admonish.nix { };
  mdbook_mermaid = pkgs.callPackage ./mdbook_mermaid.nix { };

  protoc = pkgs.callPackage ./protoc.nix { };

  lxutil = pkgs.callPackage ./lxutil.nix { };

  # Arch-specific packages for cross-compilation support
  openhcl_kernel_x64 = pkgs.callPackage ./openhcl_kernel_x64.nix { };
  openhcl_kernel_aarch64 = pkgs.callPackage ./openhcl_kernel_aarch64.nix { };
  openvmm_deps_x64 = pkgs.callPackage ./openvmm_deps_x64.nix { };
  openvmm_deps_aarch64 = pkgs.callPackage ./openvmm_deps_aarch64.nix { };
  uefi_mu_msvm_x64 = pkgs.callPackage ./uefi_mu_msvm_x64.nix { };
  uefi_mu_msvm_aarch64 = pkgs.callPackage ./uefi_mu_msvm_aarch64.nix { };

  # Legacy single-arch packages (for backward compatibility, default to x64)
  openhcl_kernel = openhcl_kernel_x64;
  openvmm_deps = openvmm_deps_x64;
  uefi_mu_msvm = uefi_mu_msvm_x64;

  glibc_2_39_52 = import (fetchTarball
    "https://github.com/NixOS/nixpkgs/archive/ab7b6889ae9d484eed2876868209e33eb262511d.tar.gz")
    { };

  overrides = (builtins.fromTOML (builtins.readFile ./Cargo.toml));
  rustVersion = overrides.workspace.package.rust-version;
  rust = pkgs.rust-bin.stable.${rustVersion}.default.override {
    extensions = [
      "rust-src" # for rust-analyzer
      "rust-analyzer"
    ];
    targets = [
      "x86_64-unknown-linux-musl"
      "x86_64-unknown-none"
      "aarch64-unknown-linux-musl"
      "aarch64-unknown-none"
    ];
  };
in pkgs.mkShell.override { } {
  nativeBuildInputs = [
    rust
    mdbook
    mdbook_admonish
    mdbook_mermaid
    protoc
  ] ++ (with pkgs; [
    libarchive
    git
    perl
    python3
    rustup
    pkg-config
    # Cross-compilation toolchain for aarch64
    pkgsCross.aarch64-multiplatform.stdenv.cc
    # Native toolchain for x64
    gcc
    binutils
  ]);
  buildInputs = [
    pkgs.openssl.dev
  ];

  # Environment variables read by flowey when using --use-nix flag
  # Arch-specific paths for cross-compilation
  NIX_OPENVMM_DEPS_X64 = openvmm_deps_x64;
  NIX_OPENVMM_DEPS_AARCH64 = openvmm_deps_aarch64;
  NIX_OPENHCL_KERNEL_X64 = openhcl_kernel_x64;
  NIX_OPENHCL_KERNEL_AARCH64 = openhcl_kernel_aarch64;
  NIX_UEFI_MU_MSVM_X64 = "${uefi_mu_msvm_x64}/MSVM.fd";
  NIX_UEFI_MU_MSVM_AARCH64 = "${uefi_mu_msvm_aarch64}/MSVM.fd";
  NIX_PROTOC_PATH = protoc;

  # Legacy environment variables (default to x64 for backward compatibility)
  NIX_OPENVMM_DEPS = openvmm_deps;
  NIX_OPENHCL_KERNEL = openhcl_kernel;
  NIX_UEFI_MU_MSVM = "${uefi_mu_msvm}/MSVM.fd";

  # Legacy: CARGO_BUILD_ARGS is no longer needed with --use-nix flag
  # Old usage: cargo xflowey build-igvm x64 $CARGO_BUILD_ARGS
  # New usage: cargo xflowey build-igvm x64 --use-nix
  CARGO_BUILD_ARGS = "--use-local-deps --custom-openvmm-deps ${openvmm_deps} --custom-uefi=${uefi_mu_msvm}/MSVM.fd --custom-kernel ${openhcl_kernel}/vmlinux --custom-kernel-modules ${openhcl_kernel}/modules --custom-protoc ${protoc}";
  RUST_BACKTRACE = 1;
  # will probably need more than one of these for local source + dependencies.
  # RUSTFLAGS = "--remap-path-prefix =/src";
  SOURCE_DATE_EPOCH = 12345;
  # Set compiler name for the aarch64 cross-compiler wrapper script
  # Nix uses aarch64-unknown-linux-gnu-gcc, while Ubuntu uses aarch64-linux-gnu-gcc
  AARCH64_GCC = "aarch64-unknown-linux-gnu-gcc";
}
