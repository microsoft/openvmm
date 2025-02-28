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
        pub arch: CommonArch,
        pub done: WriteVar<SideEffect>,
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
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::download_gh_artifact::Node>();
        ctx.import::<flowey_lib_common::gh_workflow_id::Node>();
        ctx.import::<flowey_lib_common::git_latest_commit::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { arch, done } = request;

        // Download 2411 release IGVM files for running servicing tests
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let latest_commit = ctx.reqv(|v| flowey_lib_common::git_latest_commit::Request {
            repo_path: openvmm_repo_path.clone(),
            branch: "release/2411".into(),
            commit: v,
        });

        let run = ctx.reqv(|v| flowey_lib_common::gh_workflow_id::Request {
            github_commit_hash: latest_commit,
            repo_path: openvmm_repo_path,
            pipeline_name: "[flowey] OpenVMM CI".into(),
            gh_workflow: v,
        });

        let run_id = run.map(ctx, |r| r.id);

        let mut downloaded = Vec::new();
        for arch in ["x64", "aarch64"] {
            downloaded.push(
                ctx.reqv(|v| flowey_lib_common::download_gh_artifact::Request {
                    repo_owner: "microsoft".into(),
                    repo_name: "openvmm".into(),
                    file_name: format!("{arch}-openhcl-igvm").into(),
                    path: v,
                    run_id: run_id.clone(),
                }),
            );
        }

        let mut deps = vec![ctx.reqv(crate::init_openvmm_magicpath_protoc::Request)];

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

        deps.push(
            ctx.emit_rust_step("copy downloaded release igvm files", |ctx| {
                let downloaded = downloaded.claim(ctx);
                |rt| {
                    for directory in downloaded {
                        let directory = rt.read(directory);
                        println!("downloaded to {:?}", directory);
                    }

                    Ok(())
                }
            }),
        );

        ctx.emit_side_effect_step(deps, [done]);

        Ok(())
    }
}
