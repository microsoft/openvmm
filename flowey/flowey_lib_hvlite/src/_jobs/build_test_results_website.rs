// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the test-results website using npm.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<crate::install_openvmm_rust_build_essential::Node>();
        ctx.import::<flowey_lib_common::install_nodejs::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { done } = request;

        // Make sure that npm is installed
        let npm_installed = ctx.reqv(flowey_lib_common::install_nodejs::Request::EnsureInstalled);
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step(format!("build test-results website"), |ctx| {
            npm_installed.claim(ctx);
            done.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);
            move |rt| {
                let sh = xshell::Shell::new()?;
                let mut path = rt.read(openvmm_repo_path);

                // Navigate to the petri/logview_new directory within the
                // OpenVMM repo
                path.push("petri");
                path.push("logview_new");

                sh.change_dir(&path);

                // Because the project is using vite, the output will go
                // directly to the 'dist' folder
                xshell::cmd!(sh, "npm install").run()?;
                xshell::cmd!(sh, "npm run build").run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
