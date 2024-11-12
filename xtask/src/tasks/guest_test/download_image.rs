// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use anyhow::Context;
use clap::Parser;
use clap::ValueEnum;
use std::path::PathBuf;
use std::process::Command;
use vmm_test_images::KnownIso;
use vmm_test_images::KnownVhd;

/// Download an image from Azure Blob Storage.
///
/// If no specific images are specified this command will download all available images.
#[derive(Parser)]
pub struct DownloadImageTask {
    /// The folder to download the images to.
    #[clap(short, long, default_value = "images")]
    output_folder: PathBuf,
    /// The VHDs to download.
    #[clap(long)]
    vhds: Vec<KnownVhd>,
    /// The ISOs to download.
    #[clap(long)]
    isos: Vec<KnownIso>,
    /// Redownload images even if the file already exists.
    #[clap(short, long)]
    force: bool,
}

const STORAGE_ACCOUNT: &str = "hvlitetestvhds";
const CONTAINER: &str = "vhds";

impl Xtask for DownloadImageTask {
    fn run(mut self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        if self.vhds.is_empty() && self.isos.is_empty() {
            self.vhds = KnownVhd::value_variants().to_vec();
            self.isos = KnownIso::value_variants().to_vec();
        }

        let filenames = self
            .vhds
            .into_iter()
            .map(|x| x.filename())
            .chain(self.isos.into_iter().map(|x| x.filename()))
            .collect::<Vec<_>>();

        if !self.output_folder.exists() {
            std::fs::create_dir(&self.output_folder)?;
        }

        let vhd_list = filenames.join(";");
        run_azcopy_command(&[
            "copy",
            &format!("https://{STORAGE_ACCOUNT}.blob.core.windows.net/{CONTAINER}/*"),
            self.output_folder.to_str().unwrap(),
            "--include-path",
            &vhd_list,
            "--overwrite",
            &self.force.to_string(),
        ])?;

        Ok(())
    }
}

fn run_azcopy_command(args: &[&str]) -> anyhow::Result<Option<String>> {
    let azcopy_cmd =
        which::which("azcopy").context("Failed to find `azcopy`. Is AzCopy installed?")?;

    let mut cmd = Command::new(azcopy_cmd);
    cmd.args(args);

    let mut child = cmd.spawn().context("Failed to run `azcopy` command.")?;
    let exit = child
        .wait()
        .context("Failed to wait for 'azcopy' command.")?;
    anyhow::ensure!(exit.success(), "azcopy command failed.");
    Ok(None)
}
