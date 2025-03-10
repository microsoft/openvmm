// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::download_lxutil::LxutilArch;
use crate::download_uefi_mu_msvm::MuMsvmArch;
use crate::init_openvmm_magicpath_linux_test_kernel::OpenvmmLinuxTestKernelArch;
use crate::init_openvmm_magicpath_openhcl_sysroot::OpenvmmSysrootArch;
use crate::run_cargo_build::common::CommonArch;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request{
        pub arches: Vec<CommonArch>,
        pub done: WriteVar<SideEffect>,
        pub release_2411_artifact: ReadVar<PathBuf>,
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
            release_2411_artifact,
        } = request;

        let release_2411_igvm_files =
            ctx.reqv(crate::download_release_igvm_files::resolve::Request::Release2411);

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
                let release_2411_igvm_files = release_2411_igvm_files.claim(ctx);
                let release_2411_artifact = release_2411_artifact.claim(ctx);

                |rt| {
                    let release_2411_igvm_files = rt.read(release_2411_igvm_files);
                    let release_2411_artifact = rt.read(release_2411_artifact);

                    fs_err::create_dir(release_2411_artifact.join("aarch64"))?;
                    fs_err::create_dir(release_2411_artifact.join("x64"))?;

                    fs_err::copy(
                        release_2411_igvm_files.aarch64_bin,
                        release_2411_artifact
                            .join("aarch64")
                            .join("release-2411-aarch64-openhcl.bin"),
                    )?;

                    fs_err::copy(
                        release_2411_igvm_files.x64_bin,
                        release_2411_artifact
                            .join("x64")
                            .join("release-2411-x64-openhcl.bin"),
                    )?;

                    fs_err::copy(
                        release_2411_igvm_files.x64_direct_bin,
                        release_2411_artifact
                            .join("x64")
                            .join("release-2411-x64-openhcl-direct.bin"),
                    )?;

                    Ok(())
                }
            },
        ));

        ctx.emit_side_effect_step(deps, [done]);

        Ok(())
    }
}
