// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure that Nix is installed and `nix-shell` is available on the `$PATH`.

use flowey::node::prelude::*;

flowey_request! {
    pub enum Request {
        /// Ensure Nix is installed and `nix-shell` is available on `$PATH`.
        EnsureInstalled(WriteVar<SideEffect>),
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut ensure_installed = Vec::new();

        for req in requests {
            match req {
                Request::EnsureInstalled(v) => ensure_installed.push(v),
            }
        }

        if ensure_installed.is_empty() {
            return Ok(());
        }

        let is_installed = match ctx.backend() {
            FlowBackend::Local => ctx.emit_rust_step("ensure nix is installed", |_ctx| {
                |_rt| {
                    if which::which("nix-shell").is_err() {
                        anyhow::bail!(
                            "nix-shell not found on $PATH. \
                             Please install Nix: https://nixos.org/download/"
                        );
                    }
                    Ok(())
                }
            }),
            FlowBackend::Github | FlowBackend::Ado => {
                // Step 1: Add nix profile bin to $PATH for subsequent CI
                // steps.  This runs before the install step so that when
                // the next step starts (as a new process), nix-shell is
                // already on $PATH.
                let added_to_path = ctx.emit_rust_step("add nix profile to path", |ctx| {
                    let backend = ctx.backend();
                    move |_rt| {
                        let nix_bin = home::home_dir()
                            .context("unable to get home directory")?
                            .join(".nix-profile/bin");

                        match backend {
                            FlowBackend::Github => {
                                let gh_path_file = std::env::var("GITHUB_PATH")?;
                                let mut f =
                                    fs_err::File::options().append(true).open(gh_path_file)?;
                                use std::io::Write;
                                writeln!(f, "{}", nix_bin.display())?;
                                log::info!("added {} to $GITHUB_PATH", nix_bin.display());
                            }
                            FlowBackend::Ado => {
                                println!("##vso[task.prependpath]{}", nix_bin.display());
                                log::info!("added {} to PATH (ADO)", nix_bin.display());
                            }
                            FlowBackend::Local => unreachable!(),
                        }

                        Ok(())
                    }
                });

                ctx.emit_rust_step("install nix", |ctx| {
                    added_to_path.claim(ctx);
                    move |_rt| {
                        if which::which("nix-shell").is_ok() {
                            log::info!("nix-shell already available, skipping install");
                            return Ok(());
                        }

                        log::info!("installing Nix package manager...");

                        // Single-user install (no daemon) — simplest for CI.
                        // https://nixos.org/download/
                        #[expect(clippy::disallowed_methods, reason = "can't use the nix xshell wrapper if nix isn't installed yet")]
                        let sh = xshell::Shell::new()?;
                        #[expect(clippy::disallowed_macros, reason = "can't use the nix xshell wrapper if nix isn't installed yet")]
                        xshell::cmd!(
                            sh,
                            "sh -c 'curl --proto =https --tlsv1.2 -sSf -L https://nixos.org/nix/install | sh -s -- --no-daemon'"
                        )
                        .run()?;

                        log::info!("nix installed successfully");
                        Ok(())
                    }
                })
            }
        };

        ctx.emit_side_effect_step([is_installed], ensure_installed);

        Ok(())
    }
}
