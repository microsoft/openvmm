// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Xtask;
use anyhow::Context;
use clap::Parser;
use petri_artifacts_vmm_test::ErasedVmmTestImage;
use petri_artifacts_vmm_test::parse_vmm_test_image;
use petri_artifacts_vmm_test::vmm_test_images;
use std::path::PathBuf;
use std::process::Command;

/// Download an image from Azure Blob Storage.
///
/// If no specific images are specified this command will download all available images.
#[derive(Parser)]
pub struct DownloadImageTask {
    /// The folder to download the images to.
    #[clap(short, long, default_value = "images")]
    output_folder: PathBuf,
    /// The test artifacts to download.
    #[clap(long, value_parser = parse_vmm_test_image)]
    artifacts: Vec<ErasedVmmTestImage>,
    /// Redownload images even if the file already exists.
    #[clap(short, long)]
    force: bool,
}

impl Xtask for DownloadImageTask {
    fn run(mut self, _ctx: crate::XtaskCtx) -> anyhow::Result<()> {
        if self.artifacts.is_empty() {
            self.artifacts = vmm_test_images().to_vec();
        }

        let filenames = self
            .artifacts
            .into_iter()
            .map(|x| x.filename())
            .collect::<Vec<_>>();

        if !self.output_folder.exists() {
            std::fs::create_dir(&self.output_folder)?;
        }

        let vhd_list = filenames.join(";");
        run_azcopy_command(&[
            "copy",
            &petri_artifacts_vmm_test::artifacts::blob_storage_url(),
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
