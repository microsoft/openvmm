// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install `nodejs`

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the install_nodejs node.
    pub struct Config {
        /// Which version of nodejs to install (e.g: `6.0.0`)
        pub version: Option<String>,
        /// Automatically install all required nodejs tools and components.
        ///
        /// This must be set to true/false when running locally.
        pub auto_install: Option<bool>,
    }
}

flowey_request! {
    pub enum Request {
        /// Ensure node is installed
        EnsureInstalled(WriteVar<SideEffect>),
    }
}

new_flow_node_with_config!(struct Node);

impl FlowNodeWithConfig for Node {
    type Request = Request;
    type Config = Config;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<crate::ado_task_npm_authenticate::Node>();
    }

    fn emit(
        config: Config,
        requests: Vec<Self::Request>,
        ctx: &mut NodeCtx<'_>,
    ) -> anyhow::Result<()> {
        let mut done = Vec::new();

        for req in requests {
            match req {
                Request::EnsureInstalled(v) => done.push(v),
            }
        }

        // don't require specifying a NodeVersion if no one requested node to be
        // installed
        if done.is_empty() {
            return Ok(());
        }

        let auto_install = config.auto_install;
        let version = config
            .version
            .ok_or(anyhow::anyhow!("missing config: version"))?;
        let done = done;

        // -- end of req processing -- //

        let is_installed = match ctx.backend() {
            FlowBackend::Local => {
                let auto_install = auto_install
                    .ok_or(anyhow::anyhow!("Missing essential request: AutoInstall"))?;

                let check_nodejs_install = {
                    move |_: &mut RustRuntimeServices<'_>| {
                        if which::which("node").is_err() {
                            anyhow::bail!("did not find `node` on $PATH");
                        }

                        // FUTURE: we should also be performing version checks
                        //
                        // FUTURE: check if `nvm` is available, and if so, hook
                        // into `nvm` infra to check for the node version
                        // (instead of just relying on whatever `node` is
                        // currently on the $PATH)

                        anyhow::Ok(())
                    }
                };

                if auto_install {
                    ctx.emit_rust_step("installing nodejs", |_vars| {
                        move |rt| {
                            if check_nodejs_install(rt).is_ok() {
                                return Ok(());
                            }

                            log::warn!("automatic nodejs installation is not supported yet!");
                            log::warn!(
                                "follow the guide, and manually ensure you have nodejs installed"
                            );
                            log::warn!("  ensure you have nodejs version {version} installed");
                            log::warn!("press <enter> to continue");
                            let _ = std::io::stdin().read_line(&mut String::new());

                            check_nodejs_install(rt)?;
                            Ok(())
                        }
                    })
                } else {
                    ctx.emit_rust_step("detecting nodejs install", |_vars| {
                        move |rt| {
                            check_nodejs_install(rt)?;
                            Ok(())
                        }
                    })
                }
            }
            FlowBackend::Ado => {
                if !auto_install.unwrap_or(true) {
                    anyhow::bail!("AutoInstall must be `true` when running on ADO")
                }

                let auth_done = ctx.reqv(crate::ado_task_npm_authenticate::Request::Done);

                let (did_install, claim_did_install) = ctx.new_var();
                ctx.emit_ado_step("Install nodejs", |ctx| {
                    auth_done.claim(ctx);
                    claim_did_install.claim(ctx);
                    move |_| {
                        format!(
                            r#"
                                - task: UseNode@1
                                  inputs:
                                    version: '{version}'
                            "#
                        )
                    }
                });
                did_install
            }
            FlowBackend::Github => {
                if !auto_install.unwrap_or(true) {
                    anyhow::bail!("AutoInstall must be `true` when running on Github")
                }

                ctx.emit_gh_step("Install nodejs", "actions/setup-node@v4")
                    .with("node-version", version)
                    .finish(ctx)
            }
        };

        ctx.emit_side_effect_step([is_installed], done);

        Ok(())
    }
}
