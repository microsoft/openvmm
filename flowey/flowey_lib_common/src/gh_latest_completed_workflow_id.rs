// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Gets the latest completed Github workflow id for a pipeline and branch
use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub repo: String,
        pub pipeline_name: String,
        pub branch: ReadVar<String>,
        pub gh_workflow_id: WriteVar<String>,
        pub gh_token: Option<ReadVar<String>>,
    }
}
new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::use_gh_cli::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Request {
            repo,
            gh_workflow_id,
            pipeline_name,
            branch,
            gh_token,
        } = request;

        let pipeline_name = pipeline_name.clone();

        let auth = if ctx.backend() == FlowBackend::Local {
            crate::use_gh_cli::GhCliAuth::LocalOnlyInteractive
        } else {
            let Some(token) = gh_token else {
                anyhow::bail!(
                    "Missing gh_token for non-local backend; provide a GitHub token via context when running on CI or remote backends"
                )
            };
            crate::use_gh_cli::GhCliAuth::AuthToken(token)
        };
        ctx.req(crate::use_gh_cli::Request::WithAuth(auth));

        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("get latest completed action id", |ctx| {
            let pipeline_name = pipeline_name.clone();
            let gh_cli = gh_cli.claim(ctx);
            let gh_workflow_id = gh_workflow_id.claim(ctx);
            let branch = branch.claim(ctx);

            move |rt| {
                let sh = xshell::Shell::new()?;
                let gh_cli = rt.read(gh_cli);
                let branch = rt.read(branch);

                let id = xshell::cmd!(
                    sh,
                    "{gh_cli} run list -R {repo} -b {branch} -w {pipeline_name} -s completed --limit 1 --json databaseId -q .[0].databaseId"
                )
                .read()?;

                println!("Got action id {id}");
                rt.write(gh_workflow_id, &id);

                Ok(())
            }
        });

        Ok(())
    }
}
