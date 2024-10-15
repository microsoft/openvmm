// Copyright (C) Microsoft Corporation. All rights reserved.

//! Globally install a package via `apt` on debian-based linux systems

use flowey::node::prelude::*;
use std::collections::BTreeSet;

flowey_request! {
    pub enum Request {
        /// Whether to prompt the user before installing packages
        LocalOnlyInteractive(bool),
        /// Whether to skip the `apt-update` step, and allow stale
        /// packages
        LocalOnlySkipUpdate(bool),
        /// Install the specified package(s)
        Install {
            package_names: Vec<String>,
            done: WriteVar<SideEffect>,
        },
    }
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(_ctx: &mut ImportCtx<'_>) {}

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut skip_update = None;
        let mut interactive = None;
        let mut packages = BTreeSet::new();
        let mut did_install = Vec::new();

        for req in requests {
            match req {
                Request::Install {
                    package_names,
                    done,
                } => {
                    packages.extend(package_names);
                    did_install.push(done);
                }
                Request::LocalOnlyInteractive(v) => {
                    same_across_all_reqs("LocalOnlyInteractive", &mut interactive, v)?
                }
                Request::LocalOnlySkipUpdate(v) => {
                    same_across_all_reqs("LocalOnlySkipUpdate", &mut skip_update, v)?
                }
            }
        }

        let packages = packages;
        let (skip_update, interactive) =
            if matches!(ctx.backend(), FlowBackend::Ado | FlowBackend::Github) {
                if interactive.is_some() {
                    anyhow::bail!(
                        "can only use `LocalOnlyInteractive` when using the Local backend"
                    );
                }

                if skip_update.is_some() {
                    anyhow::bail!(
                        "can only use `LocalOnlySkipUpdate` when using the Local backend"
                    );
                }

                (false, false)
            } else if matches!(ctx.backend(), FlowBackend::Local) {
                (
                    skip_update.ok_or(anyhow::anyhow!(
                        "Missing essential request: LocalOnlySkipUpdate",
                    ))?,
                    interactive.ok_or(anyhow::anyhow!(
                        "Missing essential request: LocalOnlyInteractive",
                    ))?,
                )
            } else {
                anyhow::bail!("unsupported backend")
            };

        // -- end of req processing -- //

        if did_install.is_empty() {
            return Ok(());
        }

        // maybe a questionable design choice... but we'll allow non-linux
        // platforms from taking a dep on this, and simply report that it was
        // installed.
        if !matches!(ctx.platform(), FlowPlatform::Linux) {
            ctx.emit_side_effect_step([], did_install);
            return Ok(());
        }

        let persistent_dir = ctx.persistent_dir();

        let need_install =
            ctx.emit_rust_stepv("checking if apt packages need to be installed", |ctx| {
                let persistent_dir = persistent_dir.claim(ctx);
                let packages = packages.clone();
                move |rt| {
                    // until more flowey nodes learn that debian isn't the only
                    // linux distro that exists, lets give users an escape-hatch
                    // to run linux flows on non-debian platforms.
                    if matches!(rt.backend(), FlowBackend::Local) && which::which("dpkg-query").is_err() {
                        log::error!("This Linux distribution is not actively supported at the moment.");
                        log::warn!("");
                        log::warn!("================================================================================");
                        log::warn!("You are running on an untested configuration, and may be required to manually");
                        log::warn!("install certain packages in order to build.");
                        log::warn!("");
                        log::warn!("                             PROCEED WITH CAUTION");
                        log::warn!("");
                        log::warn!("================================================================================");

                        if let Some(persistent_dir) = persistent_dir {
                            let promptfile = rt.read(persistent_dir).join("unsupported_distro_prompt");

                            if !promptfile.exists() {
                                log::info!("Press [enter] to proceed, or [ctrl-c] to exit.");
                                log::info!("This interactive prompt will only appear once.");
                                let _ = std::io::stdin().read_line(&mut String::new());
                                fs_err::write(promptfile, [])?;
                            }
                        }

                        log::warn!("Proceeding anyways...");
                        return Ok(false)
                    }

                    let sh = xshell::Shell::new()?;

                    let mut installed_packages = BTreeSet::new();
                    let packages_to_check = &packages;

                    let fmt = "${binary:Package}\n";
                    let output = xshell::cmd!(sh, "dpkg-query -W -f={fmt} {packages_to_check...}")
                        .ignore_status()
                        .output()?;
                    let output = String::from_utf8(output.stdout)?;
                    for ln in output.trim().lines() {
                        let package = match ln.split_once(':') {
                            Some((package, _arch)) => package,
                            None => ln,
                        };
                        let no_existing = installed_packages.insert(package.to_owned());
                        assert!(no_existing);
                    }

                    // apt won't re-install packages that are already
                    // up-to-date, so this sort of coarse-grained signal should
                    // be plenty sufficient.
                    Ok(installed_packages != packages)
                }
            });

        ctx.emit_rust_step("installing `apt` packages", move |ctx| {
            let packages = packages.clone();
            let need_install = need_install.claim(ctx);
            did_install.claim(ctx);
            move |rt| {
                let need_install = rt.read(need_install);

                if !need_install {
                    return Ok(());
                }

                let sh = xshell::Shell::new()?;

                if !skip_update {
                    xshell::cmd!(sh, "sudo apt-get update").run()?;
                }
                let auto_accept = (!interactive).then_some("-y");
                xshell::cmd!(sh, "sudo apt-get install {auto_accept...} {packages...}").run()?;

                Ok(())
            }
        });

        Ok(())
    }
}
