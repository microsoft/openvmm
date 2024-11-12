// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A node which returns a PathBuf to the "magic path" where OpenVMM code
//! expects certain binary dependencies to be symlinked / extracted into.
//!
//! NOTE: This must remain a separate node, as out-of-tree pipelines will
//! override this node to specify a different magicpath when building openhcl
//! from an overlay repo!

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<PathBuf>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let repo_path = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);

        ctx.emit_rust_step("report openvmm magicpath dir", |ctx| {
            let repo_path = repo_path.claim(ctx);
            let requests = requests
                .into_iter()
                .map(|x| x.0.claim(ctx))
                .collect::<Vec<_>>();
            |rt| {
                let path = rt.read(repo_path).join(".packages");
                rt.write_all(requests, &path);
                Ok(())
            }
        });

        Ok(())
    }
}
