// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ADO Task Wrapper: `NuGetToolInstaller@1`

use flowey::node::prelude::*;

flowey_request! {
    pub struct Request(pub WriteVar<SideEffect>);
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        ctx.emit_ado_step("Install nuget.exe", move |ctx| {
            requests.into_iter().for_each(|x| {
                x.0.claim(ctx);
            });
            move |_| {
                // tool is known flaky, so stick a hard-coded retry count on it
                r#"
                    - task: NuGetToolInstaller@1
                      retryCountOnTaskFailure: 3
                "#
                .into()
            }
        });

        Ok(())
    }
}
