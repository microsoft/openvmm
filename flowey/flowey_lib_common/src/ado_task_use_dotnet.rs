// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ADO Task Wrapper: `UseDotNet@2`

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request {
        pub version: String,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut done = Vec::new();
        let mut version = None;

        for req in requests {
            same_across_all_reqs("version", &mut version, req.version)?;
            done.push(req.done);
        }

        if done.is_empty() {
            return Ok(());
        }

        let version = version.ok_or(anyhow::anyhow!("Missing essential request: version"))?;

        // UseDotNet@2 requires version in the format "major.minor.x" (e.g.
        // "8.0.x"), not just "8.0".
        let ado_version = if version.matches('.').count() < 2 {
            format!("{version}.x")
        } else {
            version
        };

        ctx.emit_ado_step("Install .NET SDK", move |ctx| {
            done.claim(ctx);
            move |_| {
                format!(
                    r#"
                    - task: UseDotNet@2
                      inputs:
                        packageType: sdk
                        version: '{ado_version}'
                "#
                )
            }
        });

        Ok(())
    }
}
