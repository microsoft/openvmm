// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configures the repo source required by [`crate::git_checkout_openvmm_repo`]

use flowey::node::prelude::*;

flowey_request! {
    #[derive(Clone)]
    pub struct Params {
        pub hvlite_repo_source: flowey_lib_common::git_checkout::RepoSource,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::git_checkout::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params { hvlite_repo_source } = request;

        if matches!(ctx.backend(), FlowBackend::Local) {
            ctx.config(flowey_lib_common::git_checkout::Config {
                require_local_clones: Some(!matches!(
                    hvlite_repo_source,
                    flowey_lib_common::git_checkout::RepoSource::LocalOnlyNewClone { .. }
                )),
            });
        }

        ctx.req(flowey_lib_common::git_checkout::Request::RegisterRepo {
            repo_id: "openvmm".into(),
            repo_src: hvlite_repo_source,
            allow_persist_credentials: false,
            depth: Some(1),           // shallow fetch
            pre_run_deps: Vec::new(), // no special auth required
        });

        ctx.config(crate::git_checkout_openvmm_repo::Config {
            repo_id: Some(ConfigVar(ReadVar::from_static("openvmm".into()))),
        });

        Ok(())
    }
}
