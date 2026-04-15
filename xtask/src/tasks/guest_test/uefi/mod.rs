// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use crate::shell::XtaskShell;
use clap::Parser;
use std::path::Path;
use std::path::PathBuf;

mod gpt_efi_disk;

/// Build a UEFI test image.
#[derive(Parser)]
pub struct Uefi {
    // Output disk image. If left blank, outputs disk at `<bootx64/bootaa64>.img`
    // if only one EFI boot file is provided.
    //
    // Extension determines disk type.
    //
    // Only `.img` disk image files currently supported.
    #[clap(long)]
    output: Option<PathBuf>,

    /// File to set as `bootx64.efi`. Builds `guest_test_uefi` for x64 if no file is provided (default).
    #[clap(long)]
    #[expect(clippy::option_option)]
    bootx64: Option<Option<PathBuf>>,

    /// File to set as `bootaa64.efi`. Builds `guest_test_uefi` for ARM64 if no file is provided.
    #[clap(long)]
    #[expect(clippy::option_option)]
    bootaa64: Option<Option<PathBuf>>,
}

impl Xtask for Uefi {
    fn run(self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        let mut files = Vec::new();

        // Default case: build x64
        let bootx64 = if self.bootx64.is_none() && self.bootaa64.is_none() {
            Some(None)
        } else {
            self.bootx64
        };

        if let Some(bootx64) = bootx64.as_ref() {
            if let Some(bootx64) = bootx64 {
                files.push((Path::new("efi/boot/bootx64.efi"), bootx64.as_path()));
            } else {
                let sh = XtaskShell::new()?;
                sh.cmd("cargo")
                    .args([
                        "build",
                        "-p",
                        "guest_test_uefi",
                        "--target",
                        "x86_64-unknown-uefi",
                    ])
                    .run()?;

                files.push((
                    Path::new("efi/boot/bootx64.efi"),
                    Path::new("./target/x86_64-unknown-uefi/debug/guest_test_uefi.efi"),
                ));
            }
        }

        if let Some(bootaa64) = self.bootaa64.as_ref() {
            if let Some(bootaa64) = bootaa64 {
                files.push((Path::new("efi/boot/bootaa64.efi"), bootaa64.as_path()))
            } else {
                let sh = XtaskShell::new()?;
                sh.cmd("cargo")
                    .args([
                        "build",
                        "-p",
                        "guest_test_uefi",
                        "--target",
                        "aarch64-unknown-uefi",
                    ])
                    .run()?;

                files.push((
                    Path::new("efi/boot/bootaa64.efi"),
                    Path::new("./target/aarch64-unknown-uefi/debug/guest_test_uefi.efi"),
                ));
            }
        }

        let out_img = match self.output {
            Some(path) => path,
            None => {
                if files.len() != 1 {
                    anyhow::bail!(
                        "Multiple EFI files specified. Please provide an explicit output path."
                    )
                }
                files[0].1.with_extension("img")
            }
        };

        gpt_efi_disk::create_gpt_efi_disk(&out_img, &files)?;

        Ok(())
    }
}
