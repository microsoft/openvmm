// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build the test-results website using npm.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub path: WriteVar<PathBuf>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_nodejs::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { path } = request;

        // Make sure that npm is installed
        let npm_installed = ctx.reqv(flowey_lib_common::install_nodejs::Request::EnsureInstalled);
        let openvmm_repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("build test-results website", |ctx| {
            npm_installed.claim(ctx);
            let path = path.claim(ctx);
            let openvmm_repo_path = openvmm_repo_path.claim(ctx);

            move |rt| {
                let mut dist_path = rt.read(openvmm_repo_path);

                // Navigate to the petri/logview_new directory within the
                // OpenVMM repo
                dist_path.push("petri");
                dist_path.push("logview_new");

                rt.sh.change_dir(&dist_path);

                // Because the project is using vite, the output will go
                // directly to the 'dist-ci' folder
                flowey::shell_cmd!(rt, "npm install").run()?;
                flowey::shell_cmd!(rt, "npm run build:ci").run()?;

                dist_path.push("dist-ci");
                if !dist_path.exists() {
                    anyhow::bail!(
                        "logview_new build failed. Expected 'dist-ci' directory at {:?} but it was not found.",
                        dist_path
                    );
                }

                rt.write(path, &dist_path);

                Ok(())
            }
        });

        Ok(())
    }
}
