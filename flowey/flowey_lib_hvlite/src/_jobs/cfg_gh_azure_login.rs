// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Configuration for Azure Login on Github Actions using federated credentials (OpenIDConnect).

use flowey::node::prelude::*;

flowey_request! {
    #[derive(Clone)]
    pub struct Params {
        pub client_id: GhUserSecretVar,
        pub tenant_id: GhUserSecretVar,
        pub subscription_id: GhUserSecretVar,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<flowey_lib_common::gh_task_azure_login::Node>();
    }

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            client_id,
            tenant_id,
            subscription_id,
        } = request;

        if !matches!(ctx.backend(), FlowBackend::Github) {
            return Ok(());
        }

        let client_id = ctx.get_gh_context_var().secret(client_id);
        let tenant_id = ctx.get_gh_context_var().secret(tenant_id);
        let subscription_id = ctx.get_gh_context_var().secret(subscription_id);
        let (open_id_connect, write_open_id_connect) = ctx.new_secret_var();

        ctx.emit_rust_step("Create OpenIDConnect Credentials", |ctx| {
            let client_id = client_id.claim(ctx);
            let tenant_id = tenant_id.claim(ctx);
            let subscription_id = subscription_id.claim(ctx);
            let write_open_id_connect = write_open_id_connect.claim(ctx);
            |rt| {
                let client_id = rt.read(client_id);
                let tenant_id = rt.read(tenant_id);
                let subscription_id = rt.read(subscription_id);
                rt.write(
                    write_open_id_connect,
                    &flowey_lib_common::gh_task_azure_login::OpenIDConnect {
                        client_id,
                        tenant_id,
                        subscription_id,
                    },
                );
                Ok(())
            }
        });

        ctx.req(flowey_lib_common::gh_task_azure_login::Request::Credentials(open_id_connect));
        Ok(())
    }
}
