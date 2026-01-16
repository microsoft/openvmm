// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install Shrinkwrap and its dependencies on Ubuntu.

use flowey::node::prelude::*;

flowey_request! {
    pub struct Params {
        /// Directory where shrinkwrap repo will be cloned (e.g. <out_dir>/shrinkwrap)
        pub shrinkwrap_dir: PathBuf,
        /// If true, run apt-get and pip installs (requires sudo).
        /// If false, only clones repo and writes instructions.
        pub do_installs: bool,
        /// If true, run `git pull --ff-only` if the repo already exists.
        pub update_repo: bool,
        pub done: WriteVar<SideEffect>,
    }
}

new_simple_flow_node!(struct Node);

impl SimpleFlowNode for Node {
    type Request = Params;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn process_request(request: Self::Request, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let Params {
            shrinkwrap_dir,
            do_installs,
            update_repo,
            done,
        } = request;

        ctx.emit_rust_step("install shrinkwrap", |ctx| {
            done.claim(ctx);
            move |_rt| {
                let sh = xshell::Shell::new()?;
                
                // 0) Create parent dir
                if let Some(parent) = shrinkwrap_dir.parent() {
                    fs_err::create_dir_all(parent)?;
                }

                // 1) System deps (Ubuntu)
                if do_installs {
                    log::info!("Installing system dependencies...");
                    xshell::cmd!(sh, "sudo apt-get update").run()?;
                    xshell::cmd!(sh, "sudo apt-get install -y git netcat-openbsd python3 python3-pip python3-venv telnet docker.io").run()?;
                    
                    // Setup Docker group and add current user
                    log::info!("Setting up Docker group...");
                    let username = std::env::var("USER").unwrap_or_else(|_| "vscode".to_string());
                    
                    // Create docker group (ignore error if it already exists)
                    let _ = xshell::cmd!(sh, "sudo groupadd docker").run();
                    
                    // Add user to docker group
                    xshell::cmd!(sh, "sudo usermod -aG docker {username}").run()?;
                    
                    log::warn!("Docker group membership updated. You may need to log out and log back in for docker permissions to take effect.");
                    log::warn!("Alternatively, run: newgrp docker");
                }

                // 2) Clone shrinkwrap repo first (need it for venv location)
                if !shrinkwrap_dir.exists() {
                    log::info!("Cloning Shrinkwrap repo to {}", shrinkwrap_dir.display());
                    xshell::cmd!(sh, "git clone https://git.gitlab.arm.com/tooling/shrinkwrap.git").arg(&shrinkwrap_dir).run()?;
                } else if update_repo {
                    log::info!("Updating Shrinkwrap repo...");
                    sh.change_dir(&shrinkwrap_dir);
                    xshell::cmd!(sh, "git pull --ff-only").run()?;
                }

                // 3) Create Python virtual environment and install deps
                let venv_dir = shrinkwrap_dir.join("venv");
                if do_installs {
                    if !venv_dir.exists() {
                        log::info!("Creating Python virtual environment at {}", venv_dir.display());
                        xshell::cmd!(sh, "python3 -m venv").arg(&venv_dir).run()?;
                    }
                    
                    log::info!("Installing Python dependencies in virtual environment...");
                    let pip_bin = venv_dir.join("bin").join("pip");
                    xshell::cmd!(sh, "{pip_bin} install --upgrade pip").run()?;
                    xshell::cmd!(sh, "{pip_bin} install pyyaml termcolor tuxmake").run()?;
                }

                // 4) Clone shrinkwrap repo (if not already done)
                // 4) Validate shrinkwrap entrypoint exists
                let shrinkwrap_bin_dir = shrinkwrap_dir.join("shrinkwrap");
                if !shrinkwrap_bin_dir.exists() {
                    anyhow::bail!(
                        "expected shrinkwrap directory at {}, but it does not exist",
                        shrinkwrap_bin_dir.display()
                    );
                }

                // 5) Print PATH guidance
                log::info!("Shrinkwrap repo ready at: {}", shrinkwrap_dir.display());
                log::info!("Virtual environment at: {}", venv_dir.display());
                log::info!("To use shrinkwrap in your shell:");
                log::info!("  source {}/bin/activate", venv_dir.display());
                log::info!("  export PATH={}:$PATH", shrinkwrap_bin_dir.display());
                log::info!("Or the pipeline will invoke it directly using the venv Python.");

                Ok(())
            }
        });

        Ok(())
    }
}
