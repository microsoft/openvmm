let
  # Pinned nixpkgs in 25.11 branch
  nixpkgs = fetchTarball {
    url = "https://github.com/NixOS/nixpkgs/archive/50ab793786d9de88ee30ec4e4c24fb4236fc2674.tar.gz";
    sha256 = "1s2gr5rcyqvpr58vxdcb095mdhblij9bfzaximrva2243aal3dgx";
  };
  # Pinned rust-overlay from stable branch
  rust_overlay = import (builtins.fetchTarball {
    url = "https://github.com/oxalica/rust-overlay/archive/2ef5b3362af585a83bafd34e7fc9b1f388c2e5e2.tar.gz";
    sha256 = "138a0p83qzflw8wj4a7cainqanjmvjlincx8imr3yq1b924lg9cz";
  });
  pkgs = import nixpkgs { overlays = [ rust_overlay ]; };

  # Detect host architecture
  hostArch = if pkgs.system == "aarch64-linux" then "aarch64" else "x86_64";

  # Host tools
  mdbook = pkgs.callPackage ./nix/mdbook.nix { };
  mdbook_admonish = pkgs.callPackage ./nix/mdbook_admonish.nix { };
  mdbook_mermaid = pkgs.callPackage ./nix/mdbook_mermaid.nix { };
  protoc = pkgs.callPackage ./nix/protoc.nix { };

  # Helper to get openvmm_deps and uefi_mu_msvm by architecture
  mkBaseDepsForArch = arch: {
    openvmm_deps = pkgs.callPackage ./nix/openvmm_deps.nix { targetArch = arch; };
    uefi_mu_msvm = pkgs.callPackage ./nix/uefi_mu_msvm.nix { targetArch = arch; };
  };

  # Helper to get kernel package for specific arch and variant
  mkKernel = { arch, is_dev ? false, is_cvm ? false }: pkgs.callPackage ./nix/openhcl_kernel.nix {
    targetArch = arch;
    inherit is_dev is_cvm;
  };

  # Base deps for both architectures
  x64BaseDeps = mkBaseDepsForArch "x86_64";
  aarch64BaseDeps = mkBaseDepsForArch "aarch64";

  # Kernel variants for x64
  x64Kernel = mkKernel { arch = "x86_64"; };
  x64KernelCvm = mkKernel { arch = "x86_64"; is_cvm = true; };
  x64KernelDev = mkKernel { arch = "x86_64"; is_dev = true; };
  x64KernelCvmDev = mkKernel { arch = "x86_64"; is_dev = true; is_cvm = true; };

  # Kernel variants for aarch64
  aarch64Kernel = mkKernel { arch = "aarch64"; };
  aarch64KernelDev = mkKernel { arch = "aarch64"; is_dev = true; };

  # Cross-compilers based on host architecture
  # On x64 host: provide aarch64 cross-compiler
  # On aarch64 host: provide x64 cross-compiler
  aarch64CrossGcc = pkgs.pkgsCross.aarch64-multiplatform.buildPackages.gcc;
  x64CrossGcc = pkgs.pkgsCross.gnu64.buildPackages.gcc;

  # Native gcc (for native architecture builds)
  nativeGcc = pkgs.gcc;

  crossCompilers =
    if hostArch == "x86_64" then [ aarch64CrossGcc ]
    else [ x64CrossGcc ];

  # Rust configuration
  overrides = (builtins.fromTOML (builtins.readFile ./Cargo.toml));
  rustVersionFromCargo = overrides.workspace.package.rust-version;
  # Cargo.toml uses "X.Y", rust-overlay uses "X.Y.Z"
  # Find the latest patch version available for the given MAJOR.MINOR
  availableVersions = builtins.attrNames pkgs.rust-bin.stable;
  matchingVersions = builtins.filter
    (v: pkgs.lib.hasPrefix "${rustVersionFromCargo}." v)
    availableVersions;
  rustVersion =
    if builtins.length matchingVersions == 0
    then throw "No rust version matching ${rustVersionFromCargo}.* found in rust-overlay"
    else builtins.head (builtins.sort (a: b: builtins.compareVersions a b > 0) matchingVersions);

  rust = pkgs.rust-bin.stable.${rustVersion}.default.override {
    extensions = [
      "rust-src" # for rust-analyzer
      "rust-analyzer"
    ];
    # Include both musl targets for cross-compilation
    targets = [
      "x86_64-unknown-linux-musl"
      "x86_64-unknown-none"
      "aarch64-unknown-linux-musl"
      "aarch64-unknown-none"
    ];
  };

  # Build CARGO_BUILD_ARGS for specific architecture and kernel
  # x86_64 uses vmlinux, aarch64 uses Image
  mkCargoBuildArgs = { arch, baseDeps, kernel }:
    let kernelFile = if arch == "x86_64" then "vmlinux" else "Image";
    in "--use-local-deps --custom-openvmm-deps ${baseDeps.openvmm_deps} --custom-uefi=${baseDeps.uefi_mu_msvm}/MSVM.fd --custom-kernel ${kernel}/${kernelFile} --custom-kernel-modules ${kernel}/modules --custom-protoc ${protoc}";

in pkgs.mkShell {
  nativeBuildInputs = [
    rust
    mdbook
    mdbook_admonish
    mdbook_mermaid
    protoc
    nativeGcc
  ] ++ crossCompilers ++ (with pkgs; [
    libarchive
    git
    perl
    python3
    pkg-config
    binutils
  ]);

  buildInputs = [
    pkgs.openssl.dev
  ];

  # Sysroot paths for linker wrappers (used by build_support/underhill_cross/*-underhill-musl-gcc)
  X86_64_SYSROOT = "${x64BaseDeps.openvmm_deps}";
  AARCH64_SYSROOT = "${aarch64BaseDeps.openvmm_deps}";

  # x64 recipe variants
  CARGO_BUILD_ARGS_X64 = mkCargoBuildArgs {
    arch = "x86_64";
    baseDeps = x64BaseDeps;
    kernel = x64Kernel;
  };
  CARGO_BUILD_ARGS_X64_CVM = mkCargoBuildArgs {
    arch = "x86_64";
    baseDeps = x64BaseDeps;
    kernel = x64KernelCvm;
  };
  CARGO_BUILD_ARGS_X64_DEVKERN = mkCargoBuildArgs {
    arch = "x86_64";
    baseDeps = x64BaseDeps;
    kernel = x64KernelDev;
  };
  CARGO_BUILD_ARGS_X64_CVM_DEVKERN = mkCargoBuildArgs {
    arch = "x86_64";
    baseDeps = x64BaseDeps;
    kernel = x64KernelCvmDev;
  };

  # aarch64 recipe variants
  CARGO_BUILD_ARGS_AARCH64 = mkCargoBuildArgs {
    arch = "aarch64";
    baseDeps = aarch64BaseDeps;
    kernel = aarch64Kernel;
  };
  CARGO_BUILD_ARGS_AARCH64_DEVKERN = mkCargoBuildArgs {
    arch = "aarch64";
    baseDeps = aarch64BaseDeps;
    kernel = aarch64KernelDev;
  };

  # Expose deps for reference in update-rootfs.py
  OPENVMM_DEPS_X64 = x64BaseDeps.openvmm_deps;
  OPENVMM_DEPS_AARCH64 = aarch64BaseDeps.openvmm_deps;
  NIX_PROTOC_PATH = protoc;
  NIX_OPENHCL_KERNEL_VMLINUX = "${openhcl_kernel}/vmlinux";
  NIX_OPENHCL_KERNEL_MODULES = "${openhcl_kernel}/modules";
  NIX_UEFI_MU_MSVM = "${uefi_mu_msvm}/MSVM.fd";
  RUST_BACKTRACE = 1;
  SOURCE_DATE_EPOCH = 12345;

  shellHook = ''
    # Create a temp bin directory with symlinks using the expected gcc names.
    # The linker wrappers (build_support/underhill_cross/*-underhill-musl-gcc) expect
    # aarch64-linux-gnu-gcc and x86_64-linux-gnu-gcc, but nixpkgs provides
    # aarch64-unknown-linux-gnu-gcc and x86_64-unknown-linux-gnu-gcc (for cross)
    # and just gcc (for native). objcopy is needed by run_split_debug_info.rs.
    export NIX_CC_WRAPPER_DIR=$(mktemp -d)
    ${if hostArch == "x86_64" then ''
    # On x64 host: native x64 + cross aarch64
    ln -sf ${nativeGcc}/bin/gcc $NIX_CC_WRAPPER_DIR/x86_64-linux-gnu-gcc
    ln -sf ${pkgs.binutils}/bin/objcopy $NIX_CC_WRAPPER_DIR/x86_64-linux-gnu-objcopy
    ln -sf ${aarch64CrossGcc}/bin/aarch64-unknown-linux-gnu-gcc $NIX_CC_WRAPPER_DIR/aarch64-linux-gnu-gcc
    ln -sf ${aarch64CrossGcc}/bin/aarch64-unknown-linux-gnu-objcopy $NIX_CC_WRAPPER_DIR/aarch64-linux-gnu-objcopy
    '' else ''
    # On aarch64 host: native aarch64 + cross x64
    ln -sf ${nativeGcc}/bin/gcc $NIX_CC_WRAPPER_DIR/aarch64-linux-gnu-gcc
    ln -sf ${pkgs.binutils}/bin/objcopy $NIX_CC_WRAPPER_DIR/aarch64-linux-gnu-objcopy
    ln -sf ${x64CrossGcc}/bin/x86_64-unknown-linux-gnu-gcc $NIX_CC_WRAPPER_DIR/x86_64-linux-gnu-gcc
    ln -sf ${x64CrossGcc}/bin/x86_64-unknown-linux-gnu-objcopy $NIX_CC_WRAPPER_DIR/x86_64-linux-gnu-objcopy
    ''}
    export PATH="$NIX_CC_WRAPPER_DIR:$PATH"

    echo "OpenVMM Nix Shell"
    echo "================="
    echo "Host architecture: ${hostArch}"
    echo ""
    echo "Build commands:"
    echo "  cargo xflowey build-igvm x64 \$CARGO_BUILD_ARGS_X64"
    echo "  cargo xflowey build-igvm x64-cvm \$CARGO_BUILD_ARGS_X64_CVM"
    echo "  cargo xflowey build-igvm x64-devkern \$CARGO_BUILD_ARGS_X64_DEVKERN"
    echo "  cargo xflowey build-igvm x64-cvm-devkern \$CARGO_BUILD_ARGS_X64_CVM_DEVKERN"
    echo "  cargo xflowey build-igvm aarch64 \$CARGO_BUILD_ARGS_AARCH64"
    echo "  cargo xflowey build-igvm aarch64-devkern \$CARGO_BUILD_ARGS_AARCH64_DEVKERN"
    echo ""
  '';
}
