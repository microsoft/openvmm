// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install the Nix package manager and ensure nix-shell is available on $PATH

use flowey::node::prelude::*;
use std::io::Write;

new_flow_node!(struct Node);

flowey_request! {
    pub enum Request {
        /// Automatically install Nix package manager.
        ///
        /// Only supported on Github backend.
        AutoInstall(bool),

        /// Ensure that Nix was installed and is available on the $PATH
        EnsureInstalled(WriteVar<SideEffect>),
    }
}

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        if !matches!(ctx.backend(), FlowBackend::Github) {
            anyhow::bail!("only supported on the Github backend at this time");
        }

        let mut ensure_installed = Vec::new();
        let mut auto_install = None;

        for req in requests {
            match req {
                Request::EnsureInstalled(v) => ensure_installed.push(v),
                Request::AutoInstall(v) => {
                    same_across_all_reqs("AutoInstall", &mut auto_install, v)?
                }
            }
        }

        let ensure_installed = ensure_installed;
        let auto_install =
            auto_install.ok_or(anyhow::anyhow!("Missing essential request: AutoInstall"))?;

        // -- end of req processing -- //

        if !ensure_installed.is_empty() && auto_install {
            // Add nix profile bin to PATH first
            let added_to_path = ctx.emit_rust_step("add nix profile to path", |_| {
                |_| {
                    let nix_profile_bin = home::home_dir()
                        .context("Unable to get home dir")?
                        .join(".nix-profile")
                        .join("bin");
                    let github_path = std::env::var("GITHUB_PATH")?;
                    let mut github_path = fs_err::File::options().append(true).open(github_path)?;
                    github_path.write_all(nix_profile_bin.as_os_str().as_encoded_bytes())?;
                    github_path.write_all(b"\n")?;
                    log::info!("Added {} to PATH", nix_profile_bin.display());
                    Ok(())
                }
            });

            ctx.emit_rust_step("install Nix", |ctx| {
                ensure_installed.claim(ctx);
                added_to_path.claim(ctx);

                move |_rt: &mut RustRuntimeServices<'_>| {
                    // Check if nix-shell is already available
                    if which::which("nix-shell").is_ok() {
                        log::info!("nix-shell already available on PATH");
                        return Ok(());
                    }

                    log::info!("Installing Nix package manager...");
                    let sh = xshell::Shell::new()?;

                    // Download and run the Nix installer script (single-user mode)
                    xshell::cmd!(
                        sh,
                        "sh -c 'curl --proto =https --tlsv1.2 -L https://nixos.org/nix/install | sh -s -- --no-daemon'"
                    )
                    .run()?;

                    log::info!("Nix installed successfully");
                    Ok(())
                }
            });
        }

        Ok(())
    }
}
