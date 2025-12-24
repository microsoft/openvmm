// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Encapsulates the logic of invoking `cargo xflowey build-igvm x64 --install-missing-deps`
use flowey::node::prelude::*;

use crate::_jobs::local_build_igvm::non_production_build_igvm_tool_out_name;
use crate::build_openhcl_igvm_from_recipe::OpenhclIgvmRecipe;

flowey_request! {
    pub struct Request {
        pub base_recipe: OpenhclIgvmRecipe,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::git_checkout_openvmm_repo::Node>();
        ctx.import::<flowey_lib_common::install_rust::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { base_recipe, done } = request;

        let hvlite_repo = ctx.reqv(crate::git_checkout_openvmm_repo::req::GetRepoDir);
        let rust_install = ctx.reqv(flowey_lib_common::install_rust::Request::EnsureInstalled);
        let gh_token = (ctx.backend() == FlowBackend::Github)
            .then(|| ctx.get_gh_context_var().global().token());

        let test_local = ctx.emit_rust_step(
            "test cargo xflowey build-igvm x64 --install-missing-deps",
            |ctx| {
                rust_install.claim(ctx);
                let hvlite_repo = hvlite_repo.claim(ctx);
                let gh_token = gh_token.claim(ctx);
                move |rt| {
                    let hvlite_repo = rt.read(hvlite_repo);
                    let gh_token = rt.read(gh_token);
                    let base_recipe = non_production_build_igvm_tool_out_name(&base_recipe);
                    let sh = xshell::Shell::new()?;
                    sh.change_dir(hvlite_repo);
                    let mut cmd = xshell::cmd!(
                        sh,
                        "cargo xflowey build-igvm {base_recipe} --install-missing-deps"
                    )
                    .env("I_HAVE_A_GOOD_REASON_TO_RUN_BUILD_IGVM_IN_CI", "true");
                    if let Some(gh_token) = gh_token {
                        cmd = cmd.env("GITHUB_TOKEN", gh_token);
                    }
                    cmd.run()?;
                    Ok(())
                }
            },
        );

        ctx.emit_side_effect_step([test_local], [done]);

        Ok(())
    }
}
