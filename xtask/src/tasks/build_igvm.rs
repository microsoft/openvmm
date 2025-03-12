// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use clap::Parser;
use clap::builder::PossibleValue;
use std::fmt;
use std::path::PathBuf;

#[derive(clap::ValueEnum, Copy, Clone, PartialEq, Eq, Debug)]
enum KernelPackageKind {
    /// Last known good
    Lkg,
    /// Development
    Dev,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
/// The IGVM target architecture.
pub enum BuildIgvmArch {
    /// X64
    X86_64,
    /// ARM64
    Aarch64,
}

impl BuildIgvmArch {
    /// Host architecture, other places to have that didn't look better.
    pub fn host() -> Self {
        // xtask-fmt allow-target-arch oneoff-guest-arch-impl
        if cfg!(target_arch = "x86_64") {
            BuildIgvmArch::X86_64
        }
        // xtask-fmt allow-target-arch oneoff-guest-arch-impl
        else if cfg!(target_arch = "aarch64") {
            BuildIgvmArch::Aarch64
        } else {
            panic!("Unsupported host architecture")
        }
    }

    /// String representation (what the compiler uses).
    pub fn as_str(&self) -> &'static str {
        match self {
            BuildIgvmArch::X86_64 => "x86_64",
            BuildIgvmArch::Aarch64 => "aarch64",
        }
    }
}

impl fmt::Display for BuildIgvmArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl clap::ValueEnum for BuildIgvmArch {
    fn value_variants<'a>() -> &'a [Self] {
        &[BuildIgvmArch::X86_64, BuildIgvmArch::Aarch64]
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        match self {
            BuildIgvmArch::X86_64 => Some(PossibleValue::new("x86_64").aliases(["x86-64", "x64"])),
            BuildIgvmArch::Aarch64 => Some(PossibleValue::new("aarch64").aliases(["arm64"])),
        }
    }
}

/// (DEPRECATED) use `cargo xflowey build-igvm` instead!
#[derive(Debug, Parser)]
pub struct BuildIgvm {
    /// pass `--verbose` to cargo
    #[clap(long)]
    verbose: bool,

    /// build underhill binary in release (--profile=underhill-ship) mode
    #[clap(long)]
    release: bool,

    /// don't strip underhill binary, so that perf tooling works
    #[clap(short = 'p', long, requires("release"))]
    perf: bool,

    /// Preserve debuginfo in the underhill binary in the IGVM file.
    ///
    /// This increases the VTL2 memory requirements significantly.
    #[clap(long)]
    debuginfo: bool,

    /// path to the underhill binary, none means underhill will be built.
    #[clap(long)]
    underhill: Option<PathBuf>,

    /// path to the boot loader, none means the boot loader will be built.
    #[clap(long)]
    boot_shim: Option<PathBuf>,

    /// path to the underhill profiler, none means the profiler will be built.
    #[clap(long)]
    profiler: Option<PathBuf>,

    /// path to uefi, none means the nuget package of uefi will be used.
    #[clap(long)]
    uefi: Option<PathBuf>,

    /// include the AP kernel in the IGVM file
    #[clap(long)]
    sidecar: bool,

    /// path to the AP kernel, none means the AP kernel will be built
    #[clap(long, requires = "sidecar")]
    sidecar_path: Option<PathBuf>,

    /// json manifest passed to igvmfilegen
    #[clap(short = 'm', long)]
    manifest: Option<PathBuf>,

    /// additional layers to be included in the initrd
    #[clap(long)]
    layer: Vec<String>,

    /// additional directories to be included in the initrd
    #[clap(long)]
    directory: Vec<String>,

    /// Relative path of the output file (IGVM firmware). Defaults to the name
    /// of the manifest file if not specified.
    #[clap(short = 'o', long)]
    output_name: Option<PathBuf>,

    /// Kernel package kind.
    #[clap(short = 'k', long)]
    #[clap(value_enum, default_value_t=KernelPackageKind::Lkg)]
    kernel_kind: KernelPackageKind,

    /// Path to the kernel. Defaults to the one specified in the JSON manifest.
    #[clap(long)]
    kernel: Option<PathBuf>,

    /// Path to kernel modules. Defaults to the nuget package path.
    #[clap(long)]
    kernel_modules: Option<PathBuf>,

    /// Target architecture.
    #[clap(short = 'a', long)]
    #[clap(default_value_t=BuildIgvmArch::host())]
    arch: BuildIgvmArch,

    /// Pass additional features when building `underhill`
    #[clap(long)]
    extra_features: Vec<String>,

    /// Which Linux kernel to use for VTL0. If not specified, the packaged
    /// openvmm test linux direct kernel is used.
    #[clap(long)]
    vtl0_kernel: Option<PathBuf>,
}

impl Xtask for BuildIgvm {
    #[rustfmt::skip]
    fn run(self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let _ = self;

        log::warn!("NOTE: `cargo xtask build-igvm` has been deleted!");
        log::warn!("");
        log::warn!("Please switch over to using `cargo xflowey build-igvm` instead.");
        log::warn!("");
        log::warn!("  NOTE: The new `xflowey build-igvm` CLI is INCOMPATIBLE with `xtask build-igvm`!");
        log::warn!("");
        log::warn!("    This new CLI is designed to be more consistent, user-friendly, and better-documented than the legacy `xtask build-igvm` CLI.");
        log::warn!("    Please read the docs at `cargo xflowey build-igvm --help` to learn more");
        log::warn!("");
        log::warn!("  NOTE: The new `xflowey build-igvm` command has DIFFERENT output filenames and directories!");
        log::warn!("");
        log::warn!("    Old: `target/x86_64-unknown-linux-musl/debug/underhill-cvm.bin`");
        log::warn!("    New: `flowey-out/artifacts/build-igvm/x64-cvm/openhcl-cvm-x64.bin`");
        log::warn!("");

        anyhow::bail!("`cargo xtask build-igvm` has been deleted!");
    }
}
