// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensures that the OpenVMM repo is checked out, returning references to the
//! repo's clone directory.

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the git_checkout_openvmm_repo node.
    pub struct Config {
        /// Specify which repo-id will be passed to the `git_checkout`
        /// node.
        pub repo_id: Option<ConfigVar<String>>,
    }
}

flowey_request! {
    pub enum_struct Request {
        /// Get a path to the OpenVMM repo
        GetRepoDir(pub WriteVar<PathBuf>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::git_checkout::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let repo_id = config.repo_id.context("missing config: repo_id")?.0;
        let mut reqs = Vec::new();

        for req in requests {
            match req {
                Request::GetRepoDir(req::GetRepoDir(v)) => reqs.push(v),
            }
        }

        if reqs.is_empty() {
            return Ok(());
        }

        let path = ctx.reqv(|v| flowey_lib_common::git_checkout::Request::CheckoutRepo {
            repo_id,
            repo_path: v,
            persist_credentials: false,
        });

        ctx.emit_minor_rust_step("resolve OpenVMM repo requests", move |ctx| {
            let path = path.claim(ctx);
            let vars = reqs.claim(ctx);
            move |rt| {
                let path = rt.read(path);
                for var in vars {
                    rt.write(var, &path)
                }
            }
        });

        Ok(())
    }
}
