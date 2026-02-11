// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Downloads secrets from Azure Key Vault using the Azure CLI.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct GetSecret {
        /// The name of the Azure Key Vault that contains the secrets to download.
        pub key_vault_name: String,
        /// Downloads the specified secret
        pub secret: String,
        /// Handle to the resolved secret
        pub resolved_secret: WriteVar<String>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = GetSecret;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<super::gh_task_azure_login::Node>();
        ctx.import::<super::install_azure_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut vaults_with_secrets: BTreeMap<_, BTreeMap<_, Vec<_>>> = BTreeMap::new();
        for Self::Request {
            key_vault_name,
            secret,
            resolved_secret,
        } in requests
        {
            vaults_with_secrets
                .entry(key_vault_name)
                .or_default()
                .entry(secret)
                .or_default()
                .push(resolved_secret);
        }

        let vaults_with_secrets = vaults_with_secrets;

        let ensure_log_in = ctx.reqv(super::gh_task_azure_login::Request::EnsureLogIn);
        let az_cli_bin = ctx.reqv(super::install_azure_cli::Request::GetAzureCli);

        // -- end of req processing -- //

        for (key_vault_name, secrets_and_vars) in vaults_with_secrets {
            ctx.emit_rust_step(
                format!("Downloading secrets from key vault {}", key_vault_name),
                |ctx| {
                    let az_cli_bin = az_cli_bin.clone().claim(ctx);
                    ensure_log_in.clone().claim(ctx);
                    let secrets_and_vars = secrets_and_vars.claim(ctx);

                    |rt| {
                        let az_cli_bin = rt.read(az_cli_bin);
                        let key_vault_name = key_vault_name;
                        for (secret, vars) in secrets_and_vars {
                            let secret_value = flowey::shell_cmd!(rt, "{az_cli_bin} keyvault secret show --name {secret} --vault-name {key_vault_name} --query value --output tsv").read()?;
                            for var in vars {
                                rt.write_secret(var, &secret_value);
                            }
                        }

                        Ok(())
                    }
                },
            );
        }

        Ok(())
    }
}
