// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install the Azure CLI (`az`)

use flowey::node::prelude::*;

flowey_config! {
    /// Config for the install_azure_cli node.
    pub struct Config {
        /// Which version of azure-cli to install (e.g: 2.57.0)
        pub version: Option<String>,
        /// Automatically install all required azure-cli tools and components.
        ///
        /// This must be set to true/false when running locally.
        pub auto_install: Option<bool>,
    }
}

flowey_request! {
    pub enum Request {
        /// Get a path to `az`
        GetAzureCli(WriteVar<PathBuf>),
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
        let mut get_az_cli = Vec::new();

        for req in requests {
            match req {
                Request::GetAzureCli(v) => get_az_cli.push(v),
            }
        }

        // don't require specifying a Version if no one requested az to
        // be installed
        if get_az_cli.is_empty() {
            return Ok(());
        }

        let auto_install = config.auto_install;
        let version = config
            .version
            .ok_or(anyhow::anyhow!("missing config: version"))?;
        let get_az_cli = get_az_cli;

        // -- end of req processing -- //

        let check_az_install = {
            |_rt: &RustRuntimeServices<'_>| -> anyhow::Result<PathBuf> {
                let Ok(path) = which::which("az") else {
                    anyhow::bail!("did not find `az` on $PATH");
                };

                // FUTURE: should also perform version checks...
                anyhow::Ok(path)
            }
        };

        match ctx.backend() {
            FlowBackend::Local => {
                let auto_install = auto_install
                    .ok_or(anyhow::anyhow!("Missing essential request: AutoInstall"))?;

                if auto_install {
                    ctx.emit_rust_step("installing azure-cli", |ctx| {
                        let get_az_cli = get_az_cli.claim(ctx);
                        move |rt| {
                            log::warn!("automatic azure-cli installation is not supported yet!");
                            log::warn!(
                                "follow the guide, and manually ensure you have azure-cli installed"
                            );
                            log::warn!("  ensure you have azure-cli version {version} installed");
                            log::warn!("press <enter> to continue");
                            let _ = std::io::stdin().read_line(&mut String::new());

                            let path = check_az_install(rt)?;
                            rt.write_all(get_az_cli, &path);
                            Ok(())
                        }
                    })
                } else {
                    ctx.emit_rust_step("detecting azure-cli install", |ctx| {
                        let get_az_cli = get_az_cli.claim(ctx);
                        move |rt| {
                            let path = check_az_install(rt)?;
                            rt.write_all(get_az_cli, &path);
                            Ok(())
                        }
                    })
                }
            }
            FlowBackend::Ado => {
                if !auto_install.unwrap_or(true) {
                    anyhow::bail!("AutoInstall must be `true` when running on ADO")
                }

                // FUTURE: don't assume that all ADO workers come with azure-cli
                // pre-installed.
                ctx.emit_rust_step("detecting azure-cli install", |ctx| {
                    let get_az_cli = get_az_cli.claim(ctx);
                    move |rt| {
                        let path = check_az_install(rt)?;
                        rt.write_all(get_az_cli, &path);
                        Ok(())
                    }
                })
            }
            FlowBackend::Github => {
                if !auto_install.unwrap_or(true) {
                    anyhow::bail!("AutoInstall must be `true` when running on Github Actions")
                }

                ctx.emit_rust_step("installing azure-cli", |ctx| {
                    let get_az_cli = get_az_cli.claim(ctx);
                    move |rt| {
                        if let Ok(path) = check_az_install(rt) {
                            rt.write_all(get_az_cli, &path);
                            return Ok(());
                        }
                        match rt.platform() {
                            FlowPlatform::Windows => {
                                let az_dir = rt.sh.current_dir().join("az");
                                rt.sh.create_dir(&az_dir)?;
                                rt.sh.change_dir(&az_dir);
                                flowey::shell_cmd!(
                                    rt,
                                    "curl --fail -L https://aka.ms/installazurecliwindowszipx64 -o az.zip"
                                )
                                .run()?;
                                flowey::shell_cmd!(rt, "tar -xf az.zip").run()?;
                                rt.write_all(get_az_cli, &az_dir.join("bin\\az.cmd"));
                            }
                            FlowPlatform::Linux(_) => {
                                flowey::shell_cmd!(
                                    rt,
                                    "curl --fail -sL https://aka.ms/InstallAzureCLIDeb -o InstallAzureCLIDeb.sh"
                                )
                                .run()?;
                                flowey::shell_cmd!(rt, "chmod +x ./InstallAzureCLIDeb.sh").run()?;
                                flowey::shell_cmd!(rt, "sudo ./InstallAzureCLIDeb.sh").run()?;
                                let path = check_az_install(rt)?;
                                rt.write_all(get_az_cli, &path);
                            }
                            platform => anyhow::bail!("unsupported platform {platform}"),
                        };

                        Ok(())
                    }
                })
            }
        };

        Ok(())
    }
}
