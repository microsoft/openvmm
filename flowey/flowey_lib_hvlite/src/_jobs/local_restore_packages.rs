// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::download_lxutil::LxutilArch;
use crate::download_release_igvm_files::OpenhclReleaseVersion;
use crate::download_uefi_mu_msvm::MuMsvmArch;
use crate::init_openvmm_magicpath_linux_test_kernel::OpenvmmLinuxTestKernelArch;
use crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request{
        pub arches: Vec<CommonArch>,
        pub done: WriteVar<SideEffect>,
        pub release_artifact: ReadVar<PathBuf>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::init_openvmm_magicpath_linux_test_kernel::Node>();
        ctx.import::<crate::init_openvmm_magicpath_lxutil::Node>();
        ctx.import::<crate::init_openvmm_magicpath_openhcl_sysroot::Node>();
        ctx.import::<crate::init_openvmm_magicpath_protoc::Node>();
        ctx.import::<crate::init_openvmm_magicpath_uefi_mu_msvm::Node>();
        ctx.import::<crate::download_release_igvm_files::resolve::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            arches,
            done,
            release_artifact,
        } = request;

        let latest_release_igvm_files =
            ctx.reqv(|v| crate::download_release_igvm_files::resolve::Request {
                release_igvm_files: v,
                release_version: OpenhclReleaseVersion::latest(),
            });

        let mut deps = vec![ctx.reqv(crate::init_openvmm_magicpath_protoc::Request)];

        for arch in arches {
            match arch {
                CommonArch::X86_64 => {
                    if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
                        deps.extend_from_slice(&[ctx
                            .reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                                arch: OpenvmmSysrootArch::X64,
                                path: v,
                            })
                            .into_side_effect()]);
                    }
                    deps.extend_from_slice(&[
                        ctx.reqv(|done| crate::init_openvmm_magicpath_lxutil::Request {
                            arch: LxutilArch::X86_64,
                            done,
                        }),
                        ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                            arch: MuMsvmArch::X86_64,
                            done,
                        }),
                        ctx.reqv(
                            |done| crate::init_openvmm_magicpath_linux_test_kernel::Request {
                                arch: OpenvmmLinuxTestKernelArch::X64,
                                done,
                            },
                        ),
                    ]);
                }
                CommonArch::Aarch64 => {
                    if matches!(ctx.platform(), FlowPlatform::Linux(_)) {
                        deps.extend_from_slice(&[ctx
                            .reqv(|v| crate::init_openvmm_magicpath_openhcl_sysroot::Request {
                                arch: OpenvmmSysrootArch::Aarch64,
                                path: v,
                            })
                            .into_side_effect()]);
                    }
                    deps.extend_from_slice(&[
                        ctx.reqv(|done| crate::init_openvmm_magicpath_lxutil::Request {
                            arch: LxutilArch::Aarch64,
                            done,
                        }),
                        ctx.reqv(|done| crate::init_openvmm_magicpath_uefi_mu_msvm::Request {
                            arch: MuMsvmArch::Aarch64,
                            done,
                        }),
                        ctx.reqv(
                            |done| crate::init_openvmm_magicpath_linux_test_kernel::Request {
                                arch: OpenvmmLinuxTestKernelArch::Aarch64,
                                done,
                            },
                        ),
                    ]);
                }
            }
        }

        deps.push(ctx.emit_rust_step(
            "copy downloaded release igvm files to artifact dir",
            |ctx| {
                let latest_release_igvm_files = latest_release_igvm_files.claim(ctx);
                let latest_release_artifact = release_artifact.claim(ctx);

                |rt| {
                    let latest_release_igvm_files = rt.read(latest_release_igvm_files);
                    let latest_release_artifact = rt.read(latest_release_artifact);
                    let latest_release_version = OpenhclReleaseVersion::latest();

                    fs_err::copy(
                        latest_release_igvm_files.aarch64_bin,
                        latest_release_artifact.join(
                            latest_release_version.clone().to_string() + "-aarch64-openhcl.bin",
                        ),
                    )?;

                    fs_err::copy(
                        latest_release_igvm_files.x64_bin,
                        latest_release_artifact
                            .join(latest_release_version.clone().to_string() + "-x64-openhcl.bin"),
                    )?;

                    fs_err::copy(
                        latest_release_igvm_files.x64_direct_bin,
                        latest_release_artifact.join(
                            latest_release_version.clone().to_string() + "-x64-direct-openhcl.bin",
                        ),
                    )?;

                    Ok(())
                }
            },
        ));
        ctx.emit_side_effect_step(deps, [done]);

        Ok(())
    }
}
