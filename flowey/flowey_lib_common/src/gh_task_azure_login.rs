// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Github Actions Task Wrapper: `Azure/login@v2`

use flowey::node::prelude::*;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OpenIDConnect {
    pub client_id: String,
    pub tenant_id: String,
    pub subscription_id: String,
}

flowey_config! {
    /// Config for the gh_task_azure_login node.
    pub struct Config {
        /// Credentials for login with an Azure service principal
        pub credentials: Option<ConfigVar<OpenIDConnect>>,
    }
}

flowey_request! {
    pub enum Request {
        /// Ensure logged into Azure
        EnsureLogIn(WriteVar<SideEffect>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut ensure_log_in = Vec::new();

        for req in requests {
            match req {
                Request::EnsureLogIn(v) => ensure_log_in.push(v),
            }
        }

        let credentials = config
            .credentials
            .ok_or(anyhow::anyhow!("missing config: credentials"))?;
        let ensure_log_in = ensure_log_in;

        // -- end of req processing -- //

        if ensure_log_in.is_empty() {
            return Ok(());
        }

        let (client_id, write_client_id) = ctx.new_var();
        let (tenant_id, write_tenant_id) = ctx.new_var();
        let (subscription_id, write_subscription_id) = ctx.new_var();

        ctx.emit_rust_step("Read Azure Login Credentials", |ctx| {
            let write_client_id = write_client_id.claim(ctx);
            let write_tenant_id = write_tenant_id.claim(ctx);
            let write_subscription_id = write_subscription_id.claim(ctx);
            let credentials = credentials.claim(ctx);
            |rt| {
                let OpenIDConnect {
                    client_id,
                    tenant_id,
                    subscription_id,
                } = rt.read(credentials);
                rt.write(write_client_id, &client_id);
                rt.write(write_tenant_id, &tenant_id);
                rt.write(write_subscription_id, &subscription_id);
                Ok(())
            }
        });

        let logged_in = ctx
            .emit_gh_step("Azure Login", "Azure/login@v2")
            .with("client-id", client_id)
            .with("tenant-id", tenant_id)
            .with("subscription-id", subscription_id)
            .requires_permission(GhPermission::IdToken, GhPermissionValue::Write)
            .finish(ctx);

        ctx.emit_side_effect_step([logged_in], ensure_log_in);

        Ok(())
    }
}
