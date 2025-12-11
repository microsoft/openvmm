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
  openhcl_kernel = pkgs.callPackage ./openhcl_kernel.nix { };
  openvmm_deps = pkgs.callPackage ./openvmm_deps.nix { };
  uefi_mu_msvm = pkgs.callPackage ./uefi_mu_msvm.nix { };

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
    targets = [ "x86_64-unknown-linux-musl" "x86_64-unknown-none" ];
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
  ]);
  buildInputs = [
    pkgs.openssl.dev
  ];
  CARGO_BUILD_ARGS = "--use-local-deps --custom-openvmm-deps ${openvmm_deps} --custom-uefi=${uefi_mu_msvm}/MSVM.fd --custom-kernel ${openhcl_kernel}/vmlinux --custom-kernel-modules ${openhcl_kernel}/modules --custom-protoc ${protoc}";

  NIX_OPENVMM_DEPS = openvmm_deps;
  NIX_PROTOC_PATH = protoc;
  NIX_OPENHCL_KERNEL = "${openhcl_kernel}/vmlinux";
  NIX_OPENHCL_KERNEL_MODULES = "${openhcl_kernel}/modules";
  NIX_UEFI_MU_MSVM = "${uefi_mu_msvm}/MSVM.fd";
  RUST_BACKTRACE = 1;
  # will probably need more than one of these for local source + dependencies.
  # RUSTFLAGS = "--remap-path-prefix =/src";
  SOURCE_DATE_EPOCH = 12345;
  REALGCC = "gcc";
  USING_NIX = 1;
}
