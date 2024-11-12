// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download OpenVMM VMM test images from Azure Blob Storage.
//!
//! If persistent storage is available, caches downloaded disk images locally.

use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use vmm_test_images::KnownIso;
use vmm_test_images::KnownVhd;

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
        /// Download a specific VHD to the download folder
        DownloadVhd {
            vhd: KnownVhd,
            get_path: WriteVar<PathBuf>,
        },
        /// Download a specific ISO to the download folder
        DownloadIso {
            iso: KnownIso,
            get_path: WriteVar<PathBuf>,
        },
        /// Download multiple VHDs into the download folder
        DownloadVhds(Vec<KnownVhd>),
        /// Download multiple VHDs into the download folder
        DownloadIsos(Vec<KnownIso>),
        /// Get path to folder containing all downloaded VHDs
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
        let mut vhds = BTreeMap::<_, Vec<_>>::new();
        let mut isos = BTreeMap::<_, Vec<_>>::new();
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
                Request::DownloadVhd {
                    vhd,
                    get_path: path,
                } => vhds.entry(vhd).or_default().push(path),
                Request::DownloadIso {
                    iso,
                    get_path: path,
                } => isos.entry(iso).or_default().push(path),
                Request::DownloadVhds(v) => v.into_iter().for_each(|v| {
                    vhds.entry(v).or_default();
                }),
                Request::DownloadIsos(v) => v.into_iter().for_each(|v| {
                    isos.entry(v).or_default();
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
            let vhds = vhds.keys().cloned().collect::<Vec<_>>();
            let isos = isos.keys().cloned().collect::<Vec<_>>();
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

                rt.write(write_output_folder, &output_folder);

                //
                // Check for VHDs that have already been downloaded, to see if
                // we can skip invoking azure-cli and `azcopy` entirely.
                //
                let mut skip_vhds = BTreeSet::new();
                let mut skip_isos = BTreeSet::new();
                let mut unexpected_vhds = BTreeSet::new();
                let mut unexpected_isos = BTreeSet::new();

                for e in fs_err::read_dir(&output_folder)? {
                    let e = e?;
                    if e.file_type()?.is_dir() {
                        continue;
                    }
                    let filename = e.file_name();
                    let Some(filename) = filename.to_str() else {
                        continue;
                    };

                    if let Some(vhd) = KnownVhd::from_filename(filename) {
                        let size = e.metadata()?.len();
                        let expected_size = vhd.file_size();
                        if size != expected_size {
                            log::warn!(
                                "unexpected size for {}: expected {}, found {}",
                                filename,
                                expected_size,
                                size
                            );
                            unexpected_vhds.insert(vhd);
                        } else {
                            skip_vhds.insert(vhd);
                        }
                    } else if let Some(iso) = KnownIso::from_filename(filename) {
                        let size = e.metadata()?.len();
                        let expected_size = iso.file_size();
                        if size != expected_size {
                            log::warn!(
                                "unexpected size for {}: expected {}, found {}",
                                filename,
                                expected_size,
                                size
                            );
                            unexpected_isos.insert(iso);
                        } else {
                            skip_isos.insert(iso);
                        }
                    } else {
                        continue;
                    }
                }

                if !unexpected_vhds.is_empty() || !unexpected_isos.is_empty() {
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
                            skip_vhds.extend(unexpected_vhds.iter().copied());
                            skip_isos.extend(unexpected_isos.iter().copied());
                            unexpected_vhds.clear();
                            unexpected_isos.clear();
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

                    for vhd in vhds {
                        if !skip_vhds.contains(&vhd) || unexpected_vhds.contains(&vhd) {
                            files.push((vhd.filename().to_string(), vhd.file_size()));
                        }
                    }

                    for iso in isos {
                        if !skip_isos.contains(&iso) || unexpected_isos.contains(&iso) {
                            files.push((iso.filename().to_string(), iso.file_size()));
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

        ctx.emit_rust_step("report downloaded VMM test disk images", |ctx| {
            did_download.claim(ctx);
            let vhds = vhds.claim(ctx);
            let isos = isos.claim(ctx);
            let output_folder = output_folder.claim(ctx);
            let get_download_folder = get_download_folder.claim(ctx);
            |rt| {
                let output_folder = rt.read(output_folder).absolute()?;
                for path in get_download_folder {
                    rt.write(path, &output_folder)
                }
                for (vhd, paths) in vhds {
                    for path in paths {
                        rt.write(path, &output_folder.join(vhd.filename()))
                    }
                }
                for (iso, paths) in isos {
                    for path in paths {
                        rt.write(path, &output_folder.join(iso.filename()))
                    }
                }

                Ok(())
            }
        });

        Ok(())
    }
}

#[allow(unused)]
enum AzCopyAuthMethod {
    /// Pull credentials from the Azure CLI instance running the command.
    AzureCli,
    /// Print a link to stdout and require the user to click it to authenticate.
    Device,
}

fn download_blobs_from_azure(
    // pass dummy _rt to ensure no-one accidentally calls this at graph
    // resolution time
    _rt: &mut RustRuntimeServices<'_>,
    azcopy_bin: &PathBuf,
    azcopy_auth_method: Option<AzCopyAuthMethod>,
    files_to_download: Vec<(String, u64)>,
    output_folder: &Path,
) -> anyhow::Result<()> {
    let sh = xshell::Shell::new()?;

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
        sh.set_var("AZCOPY_AUTO_LOGIN_TYPE", auth_method);
    }
    // instead of using return codes to signal success/failure,
    // azcopy forces you to parse execution logs in order to find
    // specific strings to detect if/how a copy has failed
    //
    // thanks azcopy. very cool.
    //
    // <https://learn.microsoft.com/en-us/azure/storage/common/storage-use-azcopy-configure#review-the-logs-for-errors>
    sh.set_var("AZCOPY_JOB_PLAN_LOCATION", sh.current_dir());
    sh.set_var("AZCOPY_LOG_LOCATION", sh.current_dir());

    // setting `--overwrite true` since we do our own pre-download
    // filtering
    let result = xshell::cmd!(
        sh,
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
        xshell::cmd!(
            sh,
            "df -h --output=source,fstype,size,used,avail,pcent,target -x tmpfs -x devtmpfs"
        )
        .run()?;
        let dir_contents = sh.read_dir(sh.current_dir())?;
        for log in dir_contents
            .iter()
            .filter(|p| p.extension() == Some("log".as_ref()))
        {
            println!("{}:\n{}\n", log.display(), sh.read_file(log)?);
        }
        return result.context("failed to download VMM test disk images");
    }

    Ok(())
}
