// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Check if an Azure CLI (`az`) extension is installed.
//!
//! Checks for the extension with `az extension show`, and installs it with `az extension add`
//! only if it is missing and auto-install is enabled.
//!
//! Azure CLI is required but extension installation is best-effort. If adding
//! the extension fails, it will log a warning and return false. It is up to the
//! caller to decide whether the missing extension is fatal or not.

use crate::_util::group_by;
use flowey::node::prelude::*;

flowey_config! {
    /// Config for the install_azure_cli_extension node.
    pub struct Config {
        /// Automatically install any missing azure-cli extensions.
        ///
        /// This must be set to true/false when running locally.
        pub auto_install: Option<bool>,
    }
}

/// An Azure CLI extension that can be installed by this node.
#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum AzureCliExtension {
    /// `azure-devops`
    AzureDevops,
}

impl AzureCliExtension {
    /// The name `az extension` uses for this extension.
    pub fn name(self) -> &'static str {
        match self {
            AzureCliExtension::AzureDevops => "azure-devops",
        }
    }
}

flowey_request! {
    /// Install the extension if needed, reporting whether it is available.
    pub struct Request {
        pub extension: AzureCliExtension,
        pub installed: WriteVar<bool>,
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::install_azure_cli::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let extensions = group_by(
            requests
                .into_iter()
                .map(|req| (req.extension, req.installed)),
        );

        if extensions.is_empty() {
            return Ok(());
        }

        let auto_install = config
            .auto_install
            .ok_or(anyhow::anyhow!("Missing essential request: AutoInstall"))?;

        match ctx.backend() {
            FlowBackend::Ado if !auto_install => {
                anyhow::bail!("AutoInstall must be `true` when running on ADO")
            }
            FlowBackend::Github if !auto_install => {
                anyhow::bail!("AutoInstall must be `true` when running on Github Actions")
            }
            _ => {}
        }

        let az_cli_bin = ctx.reqv(crate::install_azure_cli::Request::GetAzureCli);

        // -- end of req processing -- //

        for (extension, installed) in extensions {
            let name = extension.name();
            ctx.emit_rust_step(format!("install az extension {name}"), |ctx| {
                let az_cli_bin = az_cli_bin.clone().claim(ctx);
                let installed = installed.claim(ctx);
                move |rt| {
                    let az_cli_bin = rt.read(az_cli_bin);

                    let is_installed = match ensure_installed(rt, &az_cli_bin, name, auto_install) {
                        Ok(()) => true,
                        Err(e) => {
                            log::warn!("az extension {name} is unavailable: {e:#}");
                            false
                        }
                    };

                    rt.write_all(installed, &is_installed);
                    Ok(())
                }
            });
        }

        Ok(())
    }
}

fn ensure_installed(
    rt: &RustRuntimeServices<'_>,
    az_cli_bin: &Path,
    name: &str,
    auto_install: bool,
) -> anyhow::Result<()> {
    let installed = flowey::shell_cmd!(
        rt,
        "{az_cli_bin} extension show --name {name} --output none"
    )
    .ignore_stderr()
    .run()
    .is_ok();

    if installed {
        log::info!("az extension {name} is already installed");
    } else if !auto_install {
        anyhow::bail!(
            "az extension {name} is not installed (install it with `az extension add --name {name}`)"
        );
    } else {
        flowey::shell_cmd!(rt, "{az_cli_bin} extension add --name {name} --output none")
            .run()
            .with_context(|| format!("failed to install az extension {name}"))?;
    }

    Ok(())
}
