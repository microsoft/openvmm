// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compose an x86-64 mshv boot disk without host filesystem utilities.

#![forbid(unsafe_code)]

use clap::Parser;
use petri::mshv_image::MshvImage;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Development-only mshv_host_boot.efi built for x86_64-unknown-uefi.
    #[arg(long)]
    host_boot: PathBuf,
    /// Directory containing HvLoader.efi, lxhvloader.dll, and Windows/System32.
    #[arg(long)]
    payload: PathBuf,
    /// mshv-enabled x64 Linux kernel with EFI stub support.
    #[arg(long)]
    kernel: PathBuf,
    /// Initramfs containing /init and the desired runtime.
    #[arg(long)]
    initrd: PathBuf,
    /// Linux command line, passed literally to its EFI stub.
    #[arg(long, default_value = "console=ttyS0,115200 rdinit=/init")]
    kernel_cmdline: String,
    /// Hypervisor options passed literally to HvLoader.
    #[arg(long, default_value = mshv_host_boot::DEFAULT_HYPERVISOR_CMDLINE)]
    hypervisor_cmdline: String,
    /// New raw GPT/FAT32 image path. Existing files are never overwritten.
    #[arg(long)]
    output: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    MshvImage {
        host_boot: &args.host_boot,
        payload: &args.payload,
        kernel: &args.kernel,
        initrd: &args.initrd,
        kernel_cmdline: &args.kernel_cmdline,
        hypervisor_cmdline: &args.hypervisor_cmdline,
    }
    .build(&args.output)?;
    eprintln!("Created {}", args.output.display());
    Ok(())
}
