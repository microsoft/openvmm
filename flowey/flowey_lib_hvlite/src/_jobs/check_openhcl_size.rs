// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Verify that OpenHCL hasn't unexpectedly grown / shrank too much in size.

use crate::build_openvmm_hcl::OpenvmmHclFeature;
use crate::download_openhcl_kernel_package::OpenhclKernelPackageArch;
use crate::download_openhcl_kernel_package::OpenhclKernelPackageKind;
use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub openvmm_hcl_target: CommonTriple,
        pub xtask_target: CommonTriple,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_openvmm_hcl::Node>();
        ctx.import::<crate::build_xtask::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::download_openhcl_kernel_package::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            openvmm_hcl_target,
            xtask_target,
            done,
        } = request;

        let repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let xtask = ctx.reqv(|v| crate::build_xtask::Request {
            target: xtask_target,
            xtask: v,
        });
        let openhcl_vmm = ctx.reqv(|v| crate::build_openvmm_hcl::Request {
            build_params: crate::build_openvmm_hcl::OpenvmmHclBuildParams {
                target: openvmm_hcl_target,
                profile: crate::build_openvmm_hcl::OpenvmmHclBuildProfile::OpenvmmHclShip,
                features: [OpenvmmHclFeature::Tpm].into(),
                no_split_dbg_info: false,
            },
            openvmm_hcl_output: v,
        });

        let did_verify_usermode_size =
            ctx.emit_rust_step("verify openhcl_vmm usermode bin size", |ctx| {
                let xtask = xtask.clone().claim(ctx);
                let repo_path = repo_path.clone().claim(ctx);
                let openhcl_vmm = openhcl_vmm.claim(ctx);

                |rt| {
                    let xtask = match rt.read(xtask) {
                        crate::build_xtask::XtaskOutput::LinuxBin { bin, .. } => bin,
                        crate::build_xtask::XtaskOutput::WindowsBin { exe, .. } => exe,
                    };
                    let openhcl_vmm = rt.read(openhcl_vmm).bin;
                    let sh = xshell::Shell::new()?;
                    sh.change_dir(rt.read(repo_path));
                    xshell::cmd!(
                        sh,
                        "{xtask} verify-size
                        -t underhill-x86_64-unknown-linux-musl-release
                        -p {openhcl_vmm}
                    "
                    )
                    .run()?;
                    Ok(())
                }
            });

        let openhcl_kernel =
            ctx.reqv(
                |v| crate::download_openhcl_kernel_package::Request::GetPackage {
                    kind: OpenhclKernelPackageKind::Main,
                    arch: OpenhclKernelPackageArch::X86_64,
                    pkg: v,
                },
            );

        let did_verify_kernel_size = ctx.emit_rust_step("verify OpenHCL kernel size", |ctx| {
            let xtask = xtask.claim(ctx);
            let repo_path = repo_path.claim(ctx);
            let openhcl_kernel = openhcl_kernel.claim(ctx);
            |rt| {
                let xtask = match rt.read(xtask) {
                    crate::build_xtask::XtaskOutput::LinuxBin { bin, .. } => bin,
                    crate::build_xtask::XtaskOutput::WindowsBin { exe, .. } => exe,
                };
                let vmlinux = rt
                    .read(openhcl_kernel)
                    .join("build/native/bin/x64/vmlinux.dbg");
                let sh = xshell::Shell::new()?;
                sh.change_dir(rt.read(repo_path));
                xshell::cmd!(
                    sh,
                    "{xtask} verify-size
                        -t hcl-kernel-ship
                        -p {vmlinux}
                    "
                )
                .run()?;
                Ok(())
            }
        });

        ctx.emit_side_effect_step([did_verify_usermode_size, did_verify_kernel_size], [done]);

        Ok(())
    }
}
