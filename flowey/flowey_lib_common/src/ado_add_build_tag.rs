// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Add a single tag to the pipeline running in an ADO environment.
//!
//! Uses `az pipelines build tag add` with the job's `System.AccessToken`, since
//! OneBranch build containers block the `##vso[build.addbuildtag]` logging
//! command. Assumes `az` and its `azure-devops` extension are installed.
//!
//! Tagging is best-effort: any failure is reported as an ADO warning, and the
//! step still succeeds.
//!
//! Only Windows jobs are supported for now.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub build_tag: ReadVar<String>,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { build_tag, done } = request;

        if !matches!(ctx.backend(), FlowBackend::Ado) {
            ctx.emit_side_effect_step([build_tag.into_side_effect()], [done]);
            return Ok(());
        }

        if !matches!(ctx.platform(), FlowPlatform::Windows) {
            anyhow::bail!("ado_add_build_tag is only supported on Windows jobs");
        }

        let access_token = ctx.get_ado_variable(AdoRuntimeVar::SYSTEM_ACCESS_TOKEN);
        let collection_uri = ctx.get_ado_variable(AdoRuntimeVar::SYSTEM_COLLECTION_URI);
        let project_id = ctx.get_ado_variable(AdoRuntimeVar::SYSTEM_TEAM_PROJECT_ID);
        let build_id = ctx.get_ado_variable(AdoRuntimeVar::BUILD_BUILD_ID);

        ctx.emit_rust_step("add build tag", |ctx| {
            let build_tag = build_tag.claim(ctx);
            let access_token = access_token.claim(ctx);
            let collection_uri = collection_uri.claim(ctx);
            let project_id = project_id.claim(ctx);
            let build_id = build_id.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let build_tag = rt.read(build_tag);
                let access_token = rt.read(access_token);
                let collection_uri = rt.read(collection_uri);
                let project_id = rt.read(project_id);
                let build_id = rt.read(build_id);

                // The azure-devops extension reads its credential from
                // AZURE_DEVOPS_EXT_PAT, which keeps the token off the command
                // line.
                let result = flowey::shell_cmd!(
                    rt,
                    "az.cmd pipelines build tag add --build-id {build_id} --tags {build_tag} --org {collection_uri} --project {project_id} --output none"
                )
                .env("AZURE_DEVOPS_EXT_PAT", &access_token)
                .run();

                if let Err(e) = result {
                    println!(
                        "##vso[task.logissue type=warning]failed to add build tag {build_tag:?}: {e}"
                    );
                }

                Ok(())
            }
        });

        Ok(())
    }
}
