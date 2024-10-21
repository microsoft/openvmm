// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use clap::Parser;

mod download_image;
mod uefi;

/// Xtask to build guest-test images (for E2E VM testing)
#[derive(Parser)]
#[clap(
    about = "Utilities to prepare guest test images",
    disable_help_subcommand = true,
    after_help = r#"NOTES:

    For documentation on each subcommand, see the corresponding subcommands's help page.
"#
)]
pub struct GuestTest {
    #[clap(subcommand)]
    command: Subcommand,
}

#[derive(clap::Subcommand)]
enum Subcommand {
    Uefi(uefi::Uefi),
    DownloadImage(download_image::DownloadImageTask),
}

impl Xtask for GuestTest {
    fn run(self, ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        match self.command {
            Subcommand::Uefi(task) => task.run(ctx),
            Subcommand::DownloadImage(task) => task.run(ctx),
        }
    }
}
