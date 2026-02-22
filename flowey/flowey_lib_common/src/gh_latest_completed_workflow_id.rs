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
        } = request;

        let pipeline_name = pipeline_name.clone();

        let gh_cli = ctx.reqv(crate::use_gh_cli::Request::Get);

        ctx.emit_rust_step("get latest completed action id", |ctx| {
            let pipeline_name = pipeline_name.clone();
            let gh_cli = gh_cli.claim(ctx);
            let gh_workflow_id = gh_workflow_id.claim(ctx);
            let branch = branch.claim(ctx);

            move |rt| {
                let gh_cli = rt.read(gh_cli);
                let branch = rt.read(branch);

                let id = flowey::shell_cmd!(
                    rt,
                    "{gh_cli} run list -R {repo} -b {branch} -w {pipeline_name} -s success --limit 1 --json databaseId -q .[0].databaseId"
                )
                .read()?;

                log::info!("Got action id {id}");
                rt.write(gh_workflow_id, &id);

                Ok(())
            }
        });

        Ok(())
    }
}
