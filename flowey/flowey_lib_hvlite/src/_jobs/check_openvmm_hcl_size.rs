// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::run_cargo_build::common::{CommonArch, CommonTriple};
use flowey::node::prelude::*;
use flowey_lib_common::download_gh_artifact;

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub new_openhcl: ReadVar<PathBuf>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_xtask::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<download_gh_artifact::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            target,
            new_openhcl,
            done,
        } = request;

        let xtask = ctx.reqv(|v| crate::build_xtask::Request {
            target: target.clone(),
            xtask: v,
        });
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let file_name = match target.common_arch().unwrap() {
            CommonArch::X86_64 => "x64-openhcl-igvm-extras",
            CommonArch::Aarch64 => "aarch64-openhcl-igvm-extras",
        };

        let merge_head_artifact = ctx.reqv(|old_openhcl| download_gh_artifact::Request {
            repo_owner: "microsoft".into(),
            repo_name: "openvmm".into(),
            file_name: file_name.into(),
            path: old_openhcl,
            run_id: "12438300136".into(),
        });

        ctx.emit_rust_step("binary size comparison", |ctx| {
            done.claim(ctx);
            let xtask = xtask.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            let old_openhcl = merge_head_artifact.claim(ctx);
            let new_openhcl = new_openhcl.claim(ctx);

            move |rt| {
                let xtask = match rt.read(xtask) {
                    crate::build_xtask::XtaskOutput::LinuxBin { bin, .. } => bin,
                    crate::build_xtask::XtaskOutput::WindowsBin { exe, .. } => exe,
                };

                let old_openhcl = rt.read(old_openhcl);
                let new_openhcl = rt.read(new_openhcl);

                let arch = target.common_arch().unwrap();

                let old_path = match arch {
                    CommonArch::X86_64 => old_openhcl.join("openhcl/openhcl"),
                    CommonArch::Aarch64 => old_openhcl.join("openhcl-aarch64/openhcl"),
                };

                let new_path = match arch {
                    CommonArch::X86_64 => new_openhcl.join("openhcl/openhcl"),
                    CommonArch::Aarch64 => new_openhcl.join("openhcl-aarch64/openhcl"),
                };

                let sh = xshell::Shell::new()?;
                sh.change_dir(rt.read(openvmm_repo_path));
                xshell::cmd!(
                    sh,
                    "{xtask} verify-size --original {old_path} --new {new_path}"
                )
                .run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
