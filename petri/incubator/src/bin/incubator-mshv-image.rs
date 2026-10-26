// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Compose an x86-64 mshv boot disk without host filesystem utilities.

#![forbid(unsafe_code)]

use clap::Parser;
use petri::mshv_image::MshvImage;
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    /// Prebuilt x64 shim whose default second stage is grubx64.efi.
    #[arg(long)]
    shim: PathBuf,
    /// Prebuilt x64 GRUB with embedded modules and prefix /boot/grub2.
    #[arg(long)]
    grub: PathBuf,
    /// Directory containing HvLoader.efi, lxhvloader.dll, and Windows/System32.
    #[arg(long)]
    payload: PathBuf,
    /// mshv-enabled x64 Linux kernel with EFI stub support.
    #[arg(long)]
    kernel: PathBuf,
    /// Initramfs containing /init and the desired runtime.
    #[arg(long)]
    initrd: PathBuf,
    /// Linux command line, passed literally rather than interpreted as GRUB code.
    #[arg(long, default_value = "console=ttyS0,115200 rdinit=/init")]
    kernel_cmdline: String,
    /// New raw GPT/FAT32 image path. Existing files are never overwritten.
    #[arg(long)]
    output: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    MshvImage {
        shim: &args.shim,
        grub: &args.grub,
        payload: &args.payload,
        kernel: &args.kernel,
        initrd: &args.initrd,
        kernel_cmdline: &args.kernel_cmdline,
    }
    .build(&args.output)?;
    eprintln!("Created {}", args.output.display());
    Ok(())
}
