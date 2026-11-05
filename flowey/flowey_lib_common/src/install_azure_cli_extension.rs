// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure an Azure CLI (`az`) extension is installed.
//!
//! Checks for the extension with `az extension show`, which only inspects the
//! local install, and runs `az extension add` only if it is missing.

use flowey::node::prelude::*;
use std::collections::BTreeMap;

flowey_request! {
    pub struct Request {
        /// Name of the extension (e.g: `azure-devops`)
        pub name: String,
        pub done: WriteVar<SideEffect>,
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_azure_cli::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut extensions: BTreeMap<String, Vec<WriteVar<SideEffect>>> = BTreeMap::new();
        for Request { name, done } in requests {
            extensions.entry(name).or_default().push(done);
        }

        if extensions.is_empty() {
            return Ok(());
        }

        let az_cli_bin = ctx.reqv(crate::install_azure_cli::Request::GetAzureCli);

        // -- end of req processing -- //

        for (name, done) in extensions {
            ctx.emit_rust_step(format!("install az extension {name}"), |ctx| {
                let az_cli_bin = az_cli_bin.clone().claim(ctx);
                done.claim(ctx);
                move |rt| {
                    let az_cli_bin = rt.read(az_cli_bin);

                    let installed = flowey::shell_cmd!(
                        rt,
                        "{az_cli_bin} extension show --name {name} --output none"
                    )
                    .ignore_stderr()
                    .run()
                    .is_ok();

                    if installed {
                        log::info!("az extension {name} is already installed");
                    } else {
                        flowey::shell_cmd!(
                            rt,
                            "{az_cli_bin} extension add --name {name} --output none"
                        )
                        .run()
                        .with_context(|| format!("failed to install az extension {name}"))?;
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
