// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install git and ensure it is available on the user's $PATH

use flowey::node::prelude::*;

new_flow_node!(struct Node);

flowey_request! {
    pub enum Request {
        /// Ensure that Git was installed and is available on $PATH
        EnsureInstalled(WriteVar<SideEffect>),

        /// Automatically install Git
        LocalOnlyAutoInstall(bool),
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(dep: &mut ImportCtx<'_>) {
        dep.import::<crate::check_needs_relaunch::Node>();
        dep.import::<crate::install_dist_pkg::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut ensure_installed = Vec::new();
        let mut auto_install = None;

        for req in requests {
            match req {
                Request::EnsureInstalled(v) => ensure_installed.push(v),
                Request::LocalOnlyAutoInstall(v) => {
                    same_across_all_reqs("LocalOnlyAutoInstall", &mut auto_install, v)?
                }
            }
        }

        let ensure_installed = ensure_installed;
        let auto_install = auto_install.ok_or(anyhow::anyhow!(
            "Missing essential request: LocalOnlyAutoInstall",
        ))?;

        // -- end of req processing -- //

        if ensure_installed.is_empty() {
            return Ok(());
        }

        if auto_install {
            let (read_bin, write_bin) = ctx.new_var();
            ctx.req(crate::check_needs_relaunch::Params {
                check: read_bin,
                done: ensure_installed,
            });

            let git_installed = ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
                package_names: vec!["git".into()],
                done: v,
            });

            ctx.emit_rust_step("install git", |ctx| {
                let write_bin = write_bin.claim(ctx);
                git_installed.claim(ctx);

                |rt: &mut RustRuntimeServices<'_>| {
                    match rt.platform() {
                        FlowPlatform::Linux(_) | FlowPlatform::MacOs => {
                            rt.write(write_bin, &Some(crate::check_needs_relaunch::BinOrEnv::Bin("git".to_string())));
                            Ok(())
                        },
                        FlowPlatform::Windows => {
                            if which::which("git.exe").is_err() {
                                flowey::shell_cmd!(rt, "powershell.exe winget install --id Microsoft.Git --accept-source-agreements").run()?;
                            }

                            rt.write(write_bin, &Some(crate::check_needs_relaunch::BinOrEnv::Bin("git".to_string())));
                            Ok(())
                        },
                        platform => anyhow::bail!("unsupported platform {platform}"),
                    }
                }
            });
        } else {
            ctx.emit_rust_step("ensure git is installed", |ctx| {
                ensure_installed.claim(ctx);
                |_rt| {
                    if which::which("git").is_err() {
                        anyhow::bail!("Please install git to continue setup.");
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
