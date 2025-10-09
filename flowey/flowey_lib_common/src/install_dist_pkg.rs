// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Globally install a package via `apt` on DEB-based Linux systems,
//! or `dnf` on RPM-based ones.
//!
//! This is a temporary solution, and this file will be split in
//! two in the future to have two flowey Nodes.
//! GitHub issue: <https://github.com/microsoft/openvmm/issues/90>

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

#[derive(Debug)]
struct PackageManager {
    distro: FlowPlatformLinuxDistro,
    sh: xshell::Shell,
}

impl PackageManager {
    fn new(ctx: &NodeCtx<'_>) -> anyhow::Result<Self> {
        let distro = match ctx.platform() {
            FlowPlatform::Linux(linux_distribution) => linux_distribution,
            _ => anyhow::bail!("Unsupported platform"),
        };

        let sh = xshell::Shell::new()?;

        Ok(Self { distro, sh })
    }

    fn distro(&self) -> FlowPlatformLinuxDistro {
        self.distro
    }

    fn query_cmd(&self, packages_to_check: &BTreeSet<String>) -> anyhow::Result<BTreeSet<String>> {
        let Self { distro, sh } = self;

        let output = match distro {
            FlowPlatformLinuxDistro::Ubuntu => {
                let fmt = "${binary:Package}\n";
                xshell::cmd!(sh, "dpkg-query -W -f={fmt} {packages_to_check...}")
            }
            FlowPlatformLinuxDistro::Fedora => {
                let fmt = "%{NAME}\n";
                xshell::cmd!(sh, "rpm -q --queryformat={fmt} {packages_to_check...}")
            }
            FlowPlatformLinuxDistro::Arch => {
                xshell::cmd!(sh, "pacman -Qq {packages_to_check...}")
            }
            FlowPlatformLinuxDistro::Unknown => anyhow::bail!("Unknown Linux distribution"),
        }
        .ignore_status()
        .output()?;
        let output = String::from_utf8(output.stdout)?;

        let mut installed_packages = BTreeSet::new();
        for ln in output.trim().lines() {
            let package = match ln.split_once(':') {
                Some((package, _arch)) => package,
                None => ln,
            };
            let no_existing = installed_packages.insert(package.to_owned());
            assert!(no_existing);
        }

        Ok(installed_packages)
    }

    fn update(&self) -> anyhow::Result<()> {
        let Self { distro, sh } = self;

        match distro {
            FlowPlatformLinuxDistro::Ubuntu => xshell::cmd!(sh, "sudo apt-get update").run()?,
            FlowPlatformLinuxDistro::Fedora => xshell::cmd!(sh, "sudo dnf update").run()?,
            // Running `pacman -Sy` without a full system update can break everything; do nothing
            FlowPlatformLinuxDistro::Arch => (),
            FlowPlatformLinuxDistro::Unknown => anyhow::bail!("Unknown Linux distribution"),
        }

        Ok(())
    }

    fn install(&self, packages: &BTreeSet<String>, interactive: bool) -> anyhow::Result<()> {
        let Self { distro, sh } = self;

        match distro {
            FlowPlatformLinuxDistro::Ubuntu => {
                let mut options = Vec::new();
                if !interactive {
                    // auto accept
                    options.push("-y");
                    // Wait for dpkg locks to be released when running in CI
                    options.extend(["-o", "DPkg::Lock::Timeout=60"]);
                }
                xshell::cmd!(sh, "sudo apt-get install {options...} {packages...}").run()?;
            }
            FlowPlatformLinuxDistro::Fedora => {
                let auto_accept = (!interactive).then_some("-y");
                xshell::cmd!(sh, "sudo dnf install {auto_accept...} {packages...}").run()?;
            }
            FlowPlatformLinuxDistro::Arch => {
                let auto_accept = (!interactive).then_some("--noconfirm");
                xshell::cmd!(sh, "sudo pacman -S {auto_accept...} {packages...}").run()?;
            }
            FlowPlatformLinuxDistro::Unknown => anyhow::bail!("Unknown Linux distribution"),
        }

        Ok(())
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
        if !matches!(ctx.platform(), FlowPlatform::Linux(_)) {
            ctx.emit_side_effect_step([], did_install);
            return Ok(());
        }

        let pacman = PackageManager::new(ctx)?;
        let persistent_dir = ctx.persistent_dir();
        let need_install =
            ctx.emit_rust_stepv("checking if packages need to be installed", |ctx| {
                let persistent_dir = persistent_dir.claim(ctx);
                let packages = packages.clone();
                move |rt| {
                    // Provide the users an escape-hatch to run Linux flows on distributions that are not actively
                    // supported at the moment.
                    if matches!(rt.backend(), FlowBackend::Local) && pacman.distro() == FlowPlatformLinuxDistro::Unknown {
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

                    let packages_to_check = &packages;
                    let installed_packages  = pacman.query_cmd(packages_to_check)?;

                    // the package manager won't re-install packages that are already
                    // up-to-date, so this sort of coarse-grained signal should
                    // be plenty sufficient.
                    Ok(installed_packages != packages)
                }
            });

        let pacman = PackageManager::new(ctx)?;
        ctx.emit_rust_step("installing packages", move |ctx| {
            let packages = packages.clone();
            let need_install = need_install.claim(ctx);
            did_install.claim(ctx);
            move |rt| {
                let need_install = rt.read(need_install);

                if !need_install {
                    return Ok(());
                }
                if !skip_update {
                    // Retry on failure in CI
                    let mut i = 0;
                    while let Err(e) = pacman.update() {
                        i += 1;
                        if i == 5 || interactive {
                            return Err(e);
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
                pacman.install(&packages, interactive)?;

                Ok(())
            }
        });

        Ok(())
    }
}
