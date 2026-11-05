// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Add a single tag to the pipeline running in an ADO environment.
//!
//! `az` is resolved via [`install_azure_cli`](crate::install_azure_cli), which
//! on ADO only locates an existing install, and the `azure-devops` extension
//! via [`install_azure_cli_extension`](crate::install_azure_cli_extension).
//! The job fails if either is unavailable.
//!
//! Once both are available, tagging is best-effort: failures are reported as
//! ADO warnings, and the step still succeeds.

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

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_azure_cli::Node>();
        ctx.import::<crate::install_azure_cli_extension::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request { build_tag, done } = request;

        if !matches!(ctx.backend(), FlowBackend::Ado) {
            ctx.emit_side_effect_step([build_tag.into_side_effect()], [done]);
            return Ok(());
        }

        let az_cli_bin = ctx.reqv(crate::install_azure_cli::Request::GetAzureCli);
        let az_devops_installed = ctx.reqv(|done| crate::install_azure_cli_extension::Request {
            name: "azure-devops".into(),
            done,
        });
        let access_token = ctx.get_ado_variable(AdoRuntimeVar::SYSTEM_ACCESS_TOKEN);
        let collection_uri = ctx.get_ado_variable(AdoRuntimeVar::SYSTEM_COLLECTION_URI);
        let project_id = ctx.get_ado_variable(AdoRuntimeVar::SYSTEM_TEAM_PROJECT_ID);
        let build_id = ctx.get_ado_variable(AdoRuntimeVar::BUILD_BUILD_ID);

        ctx.emit_rust_step("add build tag", |ctx| {
            let az_cli_bin = az_cli_bin.claim(ctx);
            az_devops_installed.claim(ctx);
            let build_tag = build_tag.claim(ctx);
            let access_token = access_token.claim(ctx);
            let collection_uri = collection_uri.claim(ctx);
            let project_id = project_id.claim(ctx);
            let build_id = build_id.claim(ctx);
            done.claim(ctx);
            move |rt| {
                let az_cli_bin = rt.read(az_cli_bin);
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
                    "{az_cli_bin} pipelines build tag add --build-id {build_id} --tags {build_tag} --org {collection_uri} --project {project_id} --output none"
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
