// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! (GitHub Actions only) Check that all dependent jobs prior to this job
//! completed successfully, and in turn, succeeding / failing itself.
//!
//! Workaround for <https://github.com/orgs/community/discussions/12395>.
//!
//! Workaround itself required _another_ workaround, in order to deal with
//! <https://github.com/actions/runner/issues/2566>.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        pub did_fail_env_var: String,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            did_fail_env_var,
            done,
        } = request;

        ctx.emit_rust_step("Check if any jobs failed", |ctx| {
            done.claim(ctx);
            |_rt| {
                let did_fail = std::env::var(did_fail_env_var)?
                    .to_lowercase()
                    .parse::<bool>()?;
                if did_fail {
                    anyhow::bail!("Detected failures in one or more previous jobs!")
                }
                Ok(())
            }
        });

        Ok(())
    }
}
