// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensures that the OpenVMM repo is checked out, returning references to the
//! repo's clone directory.

use flowey::node::prelude::*;

flowey_request! {
    pub enum_struct Request {
        /// Get a path to the OpenVMM repo
        GetRepoDir(pub WriteVar<PathBuf>),
        /// (config) specify which repo-id will be passed to the `git_checkout`
        /// node. Can be used to dynamically change the OpenVMM repo source dir
        /// based on a runtime parameter (e.g: a pipeline parameter).
        SetRepoId(pub ReadVar<String>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::git_checkout::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut repo_id = None;
        let mut reqs = Vec::new();

        for req in requests {
            match req {
                Request::GetRepoDir(req::GetRepoDir(v)) => reqs.push(v),
                Request::SetRepoId(req::SetRepoId(v)) => {
                    same_across_all_reqs_backing_var("SetRepoId", &mut repo_id, v)?
                }
            }
        }

        let repo_id = repo_id.context("missing SetRepoId request")?;

        if reqs.is_empty() {
            return Ok(());
        }

        let path = ctx.reqv(|v| flowey_lib_common::git_checkout::Request::CheckoutRepo {
            repo_id,
            repo_path: v,
            persist_credentials: false,
        });

        ctx.emit_rust_step("resolve OpenVMM repo requests", move |ctx| {
            let path = path.claim(ctx);
            let vars = reqs.claim(ctx);
            move |rt| {
                let path = rt.read(path);
                for var in vars {
                    rt.write(var, &path)
                }

                Ok(())
            }
        });

        Ok(())
    }
}
