// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Download and extract the pinned VMM.Perf ARM64 guest image.

use crate::common::CommonArch;
use crate::download_vmm_perf_runtime::verify_sha256;
use anyhow::Context as _;
use flowey::node::prelude::*;
use std::collections::BTreeMap;

struct GuestImageInfo {
    archive: &'static str,
    image: &'static str,
    sha256: &'static str,
}

flowey_request! {
    pub enum Request {
        Get {
            arch: CommonArch,
            guest_image: WriteVar<PathBuf>,
        }
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::download_azcopy::Node>();
        ctx.import::<flowey_lib_common::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut requests_by_arch = BTreeMap::<_, Vec<_>>::new();
        for Request::Get { arch, guest_image } in requests {
            requests_by_arch.entry(arch).or_default().push(guest_image);
        }
        if requests_by_arch.is_empty() {
            return Ok(());
        }

        let azcopy = ctx.reqv(flowey_lib_common::download_azcopy::Request::GetAzCopy);
        let extract_deps = flowey_lib_common::_util::extract::extract_zip_if_new_deps(ctx);
        let persistent_dir = ctx.persistent_dir();

        for (arch, outputs) in requests_by_arch {
            let info = guest_image_info(ctx.platform(), arch)?;
            let url = format!(
                "https://vmmperfartifactpublic.blob.core.windows.net/vhd/ubuntu/{}",
                info.archive
            );

            ctx.emit_rust_step(
                format!("download VMM.Perf guest image ({})", info.archive),
                |ctx| {
                    let azcopy = azcopy.clone().claim(ctx);
                    let extract_deps = extract_deps.clone().claim(ctx);
                    let persistent_dir = persistent_dir.clone().claim(ctx);
                    let outputs = outputs.claim(ctx);
                    move |rt| {
                        let cache_dir = if let Some(dir) = persistent_dir {
                            rt.read(dir)
                        } else {
                            rt.sh.current_dir()
                        }
                        .join("vmm-perf")
                        .join("guest-images");
                        fs_err::create_dir_all(&cache_dir)?;

                        let archive = cache_dir.join(info.archive);
                        let azcopy = rt.read(azcopy);
                        if archive.exists()
                            && let Err(err) = verify_sha256(&archive, info.sha256)
                        {
                            log::warn!(
                                "discarding invalid cached VMM.Perf guest image {}: {err:#}",
                                archive.display()
                            );
                            fs_err::remove_file(&archive).with_context(|| {
                                format!(
                                    "failed to remove invalid cached VMM.Perf guest image {}",
                                    archive.display()
                                )
                            })?;
                        }

                        if !archive.exists() {
                            flowey::shell_cmd!(
                                rt,
                                "{azcopy} copy
                                        {url}
                                        {archive}
                                        --overwrite ifSourceNewer
                                        --skip-version-check"
                            )
                            .run()?;
                        }

                        verify_sha256(&archive, info.sha256).or_else(|err| {
                            fs_err::remove_file(&archive).with_context(|| {
                                format!(
                                    "failed to remove VMM.Perf guest image with an invalid checksum: {}",
                                    archive.display()
                                )
                            })?;
                            Err(err)
                        })?;

                        let extract_dir =
                            flowey_lib_common::_util::extract::extract_zip_if_new(
                                rt,
                                extract_deps,
                                &archive,
                                info.sha256,
                            )?;
                        let guest_image = extract_dir.join(info.image);
                        if !guest_image.is_file() {
                            fs_err::remove_dir_all(&extract_dir).with_context(|| {
                                format!(
                                    "failed to remove incomplete VMM.Perf guest image extraction {}",
                                    extract_dir.display()
                                )
                            })?;
                            anyhow::bail!(
                                "VMM.Perf guest image archive did not contain {}",
                                info.image
                            );
                        }

                        let guest_image = guest_image.absolute()?;
                        for output in outputs {
                            rt.write(output, &guest_image);
                        }
                        Ok(())
                    }
                }
            );
        }

        Ok(())
    }
}

fn guest_image_info(platform: FlowPlatform, arch: CommonArch) -> anyhow::Result<GuestImageInfo> {
    match (platform, arch) {
        (FlowPlatform::Windows, CommonArch::Aarch64) => Ok(GuestImageInfo {
            archive: "noble-server-cloudimg-arm64-fio-iperf3-vhd.zip",
            image: "noble-server-cloudimg-arm64-fio-iperf3.vhd",
            sha256: "3f37399d1963078d99bacff17417a9de10b9c3e41dec786cea74562a54614846",
        }),
        _ => anyhow::bail!("no VMM.Perf guest image for {arch:?} on {platform:?}"),
    }
}
