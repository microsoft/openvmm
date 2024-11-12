// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ADO Task Wrapper: `NuGetAuthenticate@1`

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        EnsureAuth(WriteVar<SideEffect>),
        ServiceConnection(String),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut ensure_auth = Vec::new();
        let mut all_service_connections = Vec::new();

        for req in requests {
            match req {
                Request::EnsureAuth(v) => ensure_auth.push(v),
                Request::ServiceConnection(conn) => all_service_connections.push(conn),
            }
        }

        let ensure_auth = ensure_auth;
        let all_service_connections = all_service_connections;

        // -- end of req processing -- //

        if ensure_auth.is_empty() {
            return Ok(());
        }

        ctx.emit_ado_step("Authenticate to NuGet feeds", move |ctx| {
            ensure_auth.claim(ctx);
            move |_| {
                format!(
                    r#"
                        - task: NuGetAuthenticate@1
                          inputs:
                            nuGetServiceConnections: {}
                    "#,
                    all_service_connections.join(",")
                )
            }
        });

        Ok(())
    }
}
