// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Ensure that Nix is installed and `nix-shell` is available on the `$PATH`.

use flowey::node::prelude::*;

/// Pinned Nix installer version. Update both this and
/// [`INSTALLER_SHA256`] when bumping.
const NIX_VERSION: &str = "2.34.5";

/// SHA256 hash of the install script at
/// `https://releases.nixos.org/nix/nix-{NIX_VERSION}/install`.
const INSTALLER_SHA256: &str = "56aabba6d78b930dc12d9b788263e515b3cd9dbe4dd041a6832d75d6b121b4f3";

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
                // Go ahead and add where Nix will be to $PATH so that we can find it in
                // subsequent CI steps
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

                        // Pinned Nix installer version and SHA256 hash of the
                        // install script. Update both when bumping.
                        let installer_url = format!(
                            "https://releases.nixos.org/nix/nix-{NIX_VERSION}/install"
                        );

                        log::info!("installing Nix {NIX_VERSION}...");

                        let installer_dir = tempfile::tempdir()
                            .context("failed to create temp dir for nix installer")?;
                        let installer_path = installer_dir.path().join("install.sh");

                        // Download the installer to disk so we can verify its
                        // hash before executing it.
                        #[expect(clippy::disallowed_methods, reason = "nix is not installed yet")]
                        let sh = xshell::Shell::new()?;
                        #[expect(clippy::disallowed_macros, reason = "nix is not installed yet")]
                        xshell::cmd!(
                            sh,
                            "curl --proto =https --tlsv1.2 -sSf -L {installer_url} -o {installer_path}"
                        )
                        .run()?;

                        // Verify the SHA256 hash of the downloaded script.
                        #[expect(clippy::disallowed_macros, reason = "nix is not installed yet")]
                        let actual_hash = xshell::cmd!(sh, "sha256sum {installer_path}")
                            .read()?;
                        let actual_hash = actual_hash
                            .split_whitespace()
                            .next()
                            .context("unexpected sha256sum output")?;

                        if actual_hash != INSTALLER_SHA256 {
                            anyhow::bail!(
                                "Nix installer hash mismatch!\n  \
                                 expected: {INSTALLER_SHA256}\n  \
                                 actual:   {actual_hash}"
                            );
                        }

                        log::info!("installer hash verified: {actual_hash}");

                        #[expect(clippy::disallowed_macros, reason = "nix is not installed yet")]
                        xshell::cmd!(sh, "sh {installer_path} --no-daemon")
                            .run()?;

                        log::info!("nix {NIX_VERSION} installed successfully");
                        Ok(())
                    }
                })
            }
        };

        ctx.emit_side_effect_step([is_installed], ensure_installed);

        Ok(())
    }
}
