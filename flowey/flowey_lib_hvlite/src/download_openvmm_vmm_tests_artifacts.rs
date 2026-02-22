// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download OpenVMM VMM test artifacts from Azure Blob Storage.
//!
//! If persistent storage is available, caches downloaded artifacts locally.

use flowey::node::prelude::*;
use std::collections::BTreeSet;
use vmm_test_images::KnownTestArtifacts;

const STORAGE_ACCOUNT: &str = "hvlitetestvhds";
const CONTAINER: &str = "vhds";

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CustomDiskPolicy {
    /// Allow swapping in non-standard disk image variants
    Loose,
    /// Deny swapping in non-standard disk image variants, redownloading any
    /// images that were detected as inconsistent.
    Strict,
}

flowey_request! {
    pub enum Request {
        /// Local only: if true, skips interactive prompt that warns user about
        /// downloading many gigabytes of disk images.
        LocalOnlySkipDownloadPrompt(bool),
        /// Local only: set policy when detecting a non-standard cached disk image
        LocalOnlyCustomDiskPolicy(CustomDiskPolicy),
        /// Specify a custom cache directory. By default, VHDs are cloned
        /// into a job-local temp directory.
        CustomCacheDir(PathBuf),
        /// Download test artifacts into the download folder
        Download(Vec<KnownTestArtifacts>),
        /// Get path to folder containing all downloaded artifacts
        GetDownloadFolder(WriteVar<PathBuf>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
        ctx.import::<flowey_lib_common::install_azure_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut skip_prompt = None;
        let mut custom_disk_policy = None;
        let mut test_artifacts = BTreeSet::<_>::new();
        let mut custom_cache_dir = None;
        let mut get_download_folder = Vec::new();

        for req in requests {
            match req {
                Request::LocalOnlySkipDownloadPrompt(v) => {
                    same_across_all_reqs("LocalOnlySkipDownloadPrompt", &mut skip_prompt, v)?
                }
                Request::LocalOnlyCustomDiskPolicy(v) => {
                    same_across_all_reqs("LocalOnlyCustomDiskPolicy", &mut custom_disk_policy, v)?
                }
                Request::CustomCacheDir(v) => {
                    same_across_all_reqs("CustomCacheDir", &mut custom_cache_dir, v)?
                }
                Request::Download(v) => v.into_iter().for_each(|v| {
                    test_artifacts.insert(v);
                }),
                Request::GetDownloadFolder(path) => get_download_folder.push(path),
            }
        }

        let skip_prompt = if matches!(ctx.backend(), FlowBackend::Local) {
            skip_prompt.unwrap_or(false)
        } else {
            if skip_prompt.is_some() {
                anyhow::bail!("set `LocalOnlySkipDownloadPrompt` on non-local backend")
            }
            true
        };

        let persistent_dir = ctx.persistent_dir();

        let azcopy_bin = ctx.reqv(flowey_lib_common::download_azcopy::Request::GetAzCopy);

        let (files_to_download, write_files_to_download) = ctx.new_var::<Vec<(String, u64)>>();
        let (output_folder, write_output_folder) = ctx.new_var();

        ctx.emit_rust_step("calculating required VMM tests disk images", |ctx| {
            let persistent_dir = persistent_dir.clone().claim(ctx);
            let test_artifacts = test_artifacts.into_iter().collect::<Vec<_>>();
            let write_files_to_download = write_files_to_download.claim(ctx);
            let write_output_folder = write_output_folder.claim(ctx);
            move |rt| {
                let output_folder = if let Some(dir) = custom_cache_dir {
                    dir
                } else if let Some(dir) = persistent_dir {
                    rt.read(dir)
                } else {
                    std::env::current_dir()?
                };

                rt.write(write_output_folder, &output_folder.absolute()?);

                //
                // Check for VHDs that have already been downloaded, to see if
                // we can skip invoking azure-cli and `azcopy` entirely.
                //
                let mut skip_artifacts = BTreeSet::new();
                let mut unexpected_artifacts = BTreeSet::new();

                for e in fs_err::read_dir(&output_folder)? {
                    let e = e?;
                    if e.file_type()?.is_dir() {
                        continue;
                    }
                    let filename = e.file_name();
                    let Some(filename) = filename.to_str() else {
                        continue;
                    };

                    if let Some(vhd) = KnownTestArtifacts::from_filename(filename) {
                        let size = e.metadata()?.len();
                        let expected_size = vhd.file_size();
                        if size != expected_size {
                            log::warn!(
                                "unexpected size for {}: expected {}, found {}",
                                filename,
                                expected_size,
                                size
                            );
                            unexpected_artifacts.insert(vhd);
                        } else {
                            skip_artifacts.insert(vhd);
                        }
                    } else {
                        continue;
                    }
                }

                if !unexpected_artifacts.is_empty() {
                    if custom_disk_policy.is_none() && matches!(rt.backend(), FlowBackend::Local) {
                        log::warn!(
                            r#"
================================================================================
Detected inconsistencies between expected and cached VMM test images.

  If you are trying to use the same disks used in CI, then this is not expected,
  and your cached disks are corrupt / out-of-date and need to be re-downloaded.
  Please tweak your CLI invocation / pipeline such that
  `LocalOnlyCustomDiskPolicy` is set to `CustomDiskPolicy::Strict`.

  If you manually modified or replaced disks and you would like to keep them,
  please tweak your CLI invocation / pipeline such that
  `LocalOnlyCustomDiskPolicy` is set to `CustomDiskPolicy::Loose`.
================================================================================
"#
                        );
                    }

                    match custom_disk_policy {
                        Some(CustomDiskPolicy::Loose) => {
                            skip_artifacts.extend(unexpected_artifacts.iter().copied());
                            unexpected_artifacts.clear();
                        }
                        Some(CustomDiskPolicy::Strict) => {
                            log::warn!("detected inconsistent disks. will re-download them");
                        }
                        None => {
                            anyhow::bail!("detected inconsistent disks in disk cache")
                        }
                    }
                }

                let files_to_download = {
                    let mut files = Vec::new();

                    for artifact in test_artifacts {
                        if !skip_artifacts.contains(&artifact)
                            || unexpected_artifacts.contains(&artifact)
                        {
                            files.push((artifact.filename().to_string(), artifact.file_size()));
                        }
                    }

                    // for aesthetic reasons
                    files.sort();
                    files
                };

                if !files_to_download.is_empty() {
                    //
                    // If running locally, warn the user they're about to download a
                    // _lot_ of data
                    //
                    if matches!(rt.backend(), FlowBackend::Local) {
                        let output_folder = output_folder.display();
                        let disk_image_list = files_to_download
                            .iter()
                            .map(|(name, size)| format!("  - {name} ({size})"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        let download_size: u64 =
                            files_to_download.iter().map(|(_, size)| size).sum();
                        let msg = format!(
                            r#"
================================================================================
In order to run the selected VMM tests, some (possibly large) disk images need
to be downloaded from Azure blob storage.
================================================================================
- The following disk images will be downloaded:
{disk_image_list}

- Images will be downloaded to: {output_folder}
- The total download size is: {download_size} bytes

If running locally, you can re-run with `--help` for info on how to:
- tweak the selected download folder (e.g: download images to an external HDD)
- skip this warning prompt in the future

If you're OK with starting the download, please press <enter>.
Otherwise, press `ctrl-c` to cancel the run.
================================================================================
"#
                        );
                        log::warn!("{}", msg.trim());
                        if !skip_prompt {
                            let _ = std::io::stdin().read_line(&mut String::new());
                        }
                    }
                }

                rt.write(write_files_to_download, &files_to_download);
                Ok(())
            }
        });

        let did_download = ctx.emit_rust_step("downloading VMM test disk images", |ctx| {
            let azcopy_bin = azcopy_bin.claim(ctx);
            let files_to_download = files_to_download.claim(ctx);
            let output_folder = output_folder.clone().claim(ctx);
            |rt| {
                let files_to_download = rt.read(files_to_download);
                let output_folder = rt.read(output_folder);
                let azcopy_bin = rt.read(azcopy_bin);

                if !files_to_download.is_empty() {
                    download_blobs_from_azure(
                        rt,
                        &azcopy_bin,
                        None,
                        files_to_download,
                        &output_folder,
                    )?;
                }

                Ok(())
            }
        });

        ctx.emit_minor_rust_step("report downloaded VMM test disk images", |ctx| {
            did_download.claim(ctx);
            let output_folder = output_folder.claim(ctx);
            let get_download_folder = get_download_folder.claim(ctx);
            |rt| {
                let output_folder = rt.read(output_folder);
                for path in get_download_folder {
                    rt.write(path, &output_folder)
                }
            }
        });

        Ok(())
    }
}

#[expect(dead_code)]
enum AzCopyAuthMethod {
    /// Pull credentials from the Azure CLI instance running the command.
    AzureCli,
    /// Print a link to stdout and require the user to click it to authenticate.
    Device,
}

fn download_blobs_from_azure(
    // pass dummy _rt to ensure no-one accidentally calls this at graph
    // resolution time
    rt: &mut RustRuntimeServices<'_>,
    azcopy_bin: &PathBuf,
    azcopy_auth_method: Option<AzCopyAuthMethod>,
    files_to_download: Vec<(String, u64)>,
    output_folder: &Path,
) -> anyhow::Result<()> {
    //
    // Use azcopy to download the files
    //
    let url = format!("https://{STORAGE_ACCOUNT}.blob.core.windows.net/{CONTAINER}/*");

    let include_path = files_to_download
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>()
        .join(";");

    // Translate the authentication method we're using.
    let auth_method = azcopy_auth_method.map(|x| match x {
        AzCopyAuthMethod::AzureCli => "AZCLI",
        AzCopyAuthMethod::Device => "DEVICE",
    });

    if let Some(auth_method) = auth_method {
        rt.sh.set_var("AZCOPY_AUTO_LOGIN_TYPE", auth_method);
    }
    // instead of using return codes to signal success/failure,
    // azcopy forces you to parse execution logs in order to find
    // specific strings to detect if/how a copy has failed
    //
    // thanks azcopy. very cool.
    //
    // <https://learn.microsoft.com/en-us/azure/storage/common/storage-use-azcopy-configure#review-the-logs-for-errors>
    let current_dir = rt.sh.current_dir();
    rt.sh
        .set_var("AZCOPY_JOB_PLAN_LOCATION", current_dir.clone());
    rt.sh.set_var("AZCOPY_LOG_LOCATION", current_dir.clone());

    // setting `--overwrite true` since we do our own pre-download
    // filtering
    let result = flowey::shell_cmd!(
        rt,
        "{azcopy_bin} copy
            {url}
            {output_folder}
            --include-path {include_path}
            --overwrite true
            --skip-version-check
        "
    )
    .run();

    if result.is_err() {
        flowey::shell_cmd!(
            rt,
            "df -h --output=source,fstype,size,used,avail,pcent,target -x tmpfs -x devtmpfs"
        )
        .run()?;
        let dir_contents = rt.sh.read_dir(current_dir)?;
        for log in dir_contents
            .iter()
            .filter(|p| p.extension() == Some("log".as_ref()))
        {
            println!("{}:\n{}\n", log.display(), rt.sh.read_file(log)?);
        }
        return result.context("failed to download VMM test disk images");
    }

    Ok(())
}
