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
    #[arg(long)]
    kernel_cmdline: String,
    /// Hypervisor options passed literally to HvLoader.
    #[arg(long)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn command_lines_are_required() {
        let base = [
            "incubator-mshv-image",
            "--host-boot",
            "host.efi",
            "--payload",
            "payload",
            "--kernel",
            "bzImage",
            "--initrd",
            "initrd",
            "--output",
            "image.img",
        ];
        for (missing, provided) in [
            ("--kernel-cmdline", "--hypervisor-cmdline"),
            ("--hypervisor-cmdline", "--kernel-cmdline"),
        ] {
            let error = Args::try_parse_from(base.into_iter().chain([provided, "explicit=value"]))
                .err()
                .expect("omitted command line must be rejected");
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
            assert!(error.to_string().contains(missing), "{error}");
        }
        let args = Args::try_parse_from(base.into_iter().chain([
            "--kernel-cmdline",
            "console=ttyS0 rdinit=/caller-init",
            "--hypervisor-cmdline",
            r"MSHV_ROOT=\Windows MSHV_ENABLE=TRUE",
        ]))
        .unwrap();
        assert_eq!(args.kernel_cmdline, "console=ttyS0 rdinit=/caller-init");
        assert_eq!(
            args.hypervisor_cmdline,
            r"MSHV_ROOT=\Windows MSHV_ENABLE=TRUE"
        );
    }
}
