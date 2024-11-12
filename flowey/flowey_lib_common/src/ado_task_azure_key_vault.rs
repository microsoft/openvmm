// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ADO Task Wrapper: `AzureKeyVault@2`

use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Select the service connection for the Azure subscription containing the Azure Key Vault instance, or create a new connection.
        pub subscription: String,
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
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {
        // no deps
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut vaults_with_secrets: BTreeMap<_, BTreeMap<_, Vec<_>>> = BTreeMap::new();
        for Request {
            subscription,
            key_vault_name,
            secret,
            resolved_secret,
        } in requests
        {
            vaults_with_secrets
                .entry((subscription, key_vault_name))
                .or_default()
                .entry(secret)
                .or_default()
                .push(resolved_secret);
        }

        let vaults_with_secrets = vaults_with_secrets;

        // -- end of req processing -- //

        for ((subscription, key_vault_name), secrets_and_vars) in vaults_with_secrets {
            let secrets = secrets_and_vars
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(",");

            ctx.emit_ado_step(
                format!(
                    "Downloading secrets from key vault {}/{}",
                    subscription, key_vault_name
                ),
                move |ctx| {
                    let secrets_and_vars = secrets_and_vars.claim(ctx);

                    move |rt| {
                        for (secret, out_vars) in secrets_and_vars {
                            for var in out_vars {
                                rt.set_var(
                                    var,
                                    AdoRuntimeVar::dangerous_from_global(secret.clone(), true),
                                )
                            }
                        }

                        format!(
                            r#"
                                - task: AzureKeyVault@2
                                  inputs:
                                    azureSubscription: '{subscription}'
                                    KeyVaultName: '{key_vault_name}'
                                    SecretsFilter: '{secrets}'
                            "#
                        )
                    }
                },
            );
        }

        Ok(())
    }
}
