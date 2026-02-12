// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Invoke `xtask fmt`

use crate::run_cargo_build::common::CommonTriple;
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub target: CommonTriple,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::build_xtask::Node>();
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { target, done } = request;

        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        let xtask = ctx.reqv(|v| crate::build_xtask::Request { target, xtask: v });

        ctx.emit_rust_step("run xtask fmt", |ctx| {
            done.claim(ctx);
            let xtask = xtask.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            |rt| {
                let xtask = match rt.read(xtask) {
                    crate::build_xtask::XtaskOutput::LinuxBin { bin, .. } => bin,
                    crate::build_xtask::XtaskOutput::WindowsBin { exe, .. } => exe,
                };

                let path = rt.read(openvmm_repo_path);
                rt.sh.change_dir(path);
                flowey::shell_cmd!(rt, "{xtask} fmt --no-parallel")
                    // CI runs with trace logging, but that results in a lot of
                    // spam when running the xtask flowey check
                    .env("FLOWEY_LOG", "info")
                    .run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
