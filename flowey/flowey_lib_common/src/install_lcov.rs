// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install lcov and ensure it is available on the user's $PATH

use flowey::node::prelude::*;

new_flow_node!(struct Node);

flowey_request! {
    pub enum Request {
        /// Ensure that lcov was installed and is available on $PATH
        EnsureInstalled(WriteVar<SideEffect>),

        /// Automatically install lcov
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

            let lcov_installed = ctx.reqv(|v| crate::install_dist_pkg::Request::Install {
                package_names: vec!["lcov".into()],
                done: v,
            });

            ctx.emit_rust_step("install lcov", |ctx| {
                let write_bin = write_bin.claim(ctx);
                lcov_installed.claim(ctx);

                |rt: &mut RustRuntimeServices<'_>| {
                    match rt.platform() {
                        FlowPlatform::Linux(_) => {
                            rt.write(write_bin, &Some(crate::check_needs_relaunch::BinOrEnv::Bin("lcov".to_string())));
                            Ok(())
                        },
                        FlowPlatform::MacOs => {
                            // On macOS, try to install via homebrew if available
                            if which::which("brew").is_ok() {
                                let sh = xshell::Shell::new()?;
                                xshell::cmd!(sh, "brew install lcov").run()?;
                            } else {
                                log::warn!("lcov installation on macOS requires homebrew. Please install homebrew and run 'brew install lcov'");
                            }
                            rt.write(write_bin, &Some(crate::check_needs_relaunch::BinOrEnv::Bin("lcov".to_string())));
                            Ok(())
                        },
                        FlowPlatform::Windows => {
                            log::warn!("lcov is not typically available on Windows. Consider using Windows Subsystem for Linux (WSL) for coverage reports.");
                            rt.write(write_bin, &None);
                            Ok(())
                        },
                        platform => anyhow::bail!("unsupported platform {platform}"),
                    }
                }
            });
        } else {
            ctx.emit_rust_step("ensure lcov is installed", |ctx| {
                ensure_installed.claim(ctx);
                |_rt| {
                    if which::which("lcov").is_err() {
                        anyhow::bail!("Please install lcov to continue (e.g., 'apt install lcov' on Ubuntu, 'brew install lcov' on macOS).");
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}