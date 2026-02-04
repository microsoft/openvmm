// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Install a nuget package

use crate::download_nuget_exe::NugetInstallPlatform;
use flowey::node::prelude::*;
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub struct NugetPackage {
    pub id: String,
    pub version: String,
}

flowey_request! {
    pub enum Request {
        /// A bundle of packages to install in one nuget invocation
        Install {
            /// Path to a nuget.config file
            nuget_config_file: ReadVar<PathBuf>,
            /// A list of nuget packages to install, and outvars denoting where they
            /// were extracted to.
            packages: Vec<(ReadVar<NugetPackage>, WriteVar<PathBuf>)>,
            /// Directory to install the packages into.
            install_dir: ReadVar<PathBuf>,
            /// Side effects that must have run before installing these packages.
            ///
            /// e.g: requiring that a nuget credentials manager has been installed
            pre_install_side_effects: Vec<ReadVar<SideEffect>>,
        },
        /// Whether to pass `-NonInteractive` to `nuget install`
        LocalOnlyInteractive(bool),
    }
}

struct InstallRequest {
    nuget_config_file: ReadVar<PathBuf>,
    packages: Vec<(ReadVar<NugetPackage>, WriteVar<PathBuf>)>,
    install_dir: ReadVar<PathBuf>,
    pre_install_side_effects: Vec<ReadVar<SideEffect>>,
}

new_flow_node!(struct Node);

impl FlowNode for Node {
    type Request = Request;

    fn imports(ctx: &mut ImportCtx<'_>) {
        ctx.import::<super::install_nuget_azure_credential_provider::Node>();
        ctx.import::<super::download_nuget_exe::Node>();
    }

    fn emit(requests: Vec<Self::Request>, ctx: &mut NodeCtx<'_>) -> anyhow::Result<()> {
        let mut interactive = None;
        let mut install = Vec::new();

        for request in requests {
            match request {
                Request::LocalOnlyInteractive(v) => {
                    same_across_all_reqs("LocalOnlyInteractive", &mut interactive, v)?
                }

                Request::Install {
                    packages,
                    nuget_config_file,
                    install_dir,
                    pre_install_side_effects,
                } => install.push(InstallRequest {
                    packages,
                    nuget_config_file,
                    install_dir,
                    pre_install_side_effects,
                }),
            }
        }

        let interactive = if matches!(ctx.backend(), FlowBackend::Ado | FlowBackend::Github) {
            if interactive.is_some() {
                anyhow::bail!("can only use `LocalOnlyInteractive` when using the Local backend");
            }
            false
        } else if matches!(ctx.backend(), FlowBackend::Local) {
            interactive.ok_or(anyhow::anyhow!(
                "Missing essential request: LocalOnlyInteractive",
            ))?
        } else {
            anyhow::bail!("unsupported backend")
        };

        // -- end of req processing -- //

        if install.is_empty() {
            return Ok(());
        }

        // need nuget to be installed
        let nuget_bin = ctx.reqv(super::download_nuget_exe::Request::NugetBin);

        let nuget_config_platform =
            ctx.reqv(super::download_nuget_exe::Request::NugetInstallPlatform);

        for InstallRequest {
            packages,
            nuget_config_file,
            install_dir,
            pre_install_side_effects,
        } in install
        {
            ctx.emit_rust_step("restore nuget packages", |ctx| {
                let nuget_bin = nuget_bin.clone().claim(ctx);
                let nuget_config_platform = nuget_config_platform.clone().claim(ctx);
                let install_dir = install_dir.claim(ctx);
                pre_install_side_effects.claim(ctx);

                let packages = packages
                    .into_iter()
                    .map(|(a, b)| (a.claim(ctx), b.claim(ctx)))
                    .collect::<Vec<_>>();
                let nuget_config_file = nuget_config_file.claim(ctx);

                move |rt| {
                    let nuget_bin = rt.read(nuget_bin);
                    let nuget_config_platform = rt.read(nuget_config_platform);
                    let nuget_config_file = rt.read(nuget_config_file);
                    let install_dir = rt.read(install_dir);

                    let packages = {
                        let mut pkgmap: BTreeMap<_, Vec<_>> = BTreeMap::new();
                        for (package, var) in packages {
                            pkgmap.entry(rt.read(package)).or_default().push(var);
                        }
                        pkgmap
                    };

                    // for whatever reason, unlike most other package managers,
                    // nuget doesn't actually let you pass a list of arbitrary
                    // packages to restore directly from the CLI. Unless you
                    // want to constantly re-invoke nuget.exe, you're forced to
                    // maintain a packages.config.
                    //
                    // To work around this, simply generate a packages.config on
                    // the fly.
                    let packages_config = {
                        let mut packages_config = String::new();
                        let _ =
                            writeln!(packages_config, r#"<?xml version="1.0" encoding="utf-8"?>"#);
                        let _ = writeln!(packages_config, r#"<packages>"#);
                        for NugetPackage { id, version } in packages.keys() {
                            let _ = writeln!(
                                packages_config,
                                r#"  <package id="{}" version="{}" />"#,
                                id, version
                            );
                        }
                        let _ = writeln!(packages_config, r#"</packages>"#);
                        packages_config
                    };

                    log::debug!("generated package.config:\n{}", packages_config);

                    let packages_config_filepath = PathBuf::from("./packages.config");

                    fs_err::write(&packages_config_filepath, packages_config)?;

                    // If we're crossing the WSL boundary we need to translate our config paths.
                    let (packages_config_filepath, config_filepath) =
                        if crate::_util::running_in_wsl(rt)
                            && matches!(nuget_config_platform, NugetInstallPlatform::Windows)
                        {
                            (
                                crate::_util::wslpath::linux_to_win(&packages_config_filepath),
                                crate::_util::wslpath::linux_to_win(&nuget_config_file),
                            )
                        } else {
                            (packages_config_filepath, nuget_config_file)
                        };

                    // now, run the nuget install command
                    let non_interactive = (!interactive).then_some("-NonInteractive");

                    // FUTURE: add checks to avoid having to invoke
                    // nuget at all (a-la the "expected_hashes" in the
                    // old ci/restore.sh)
                    flowey::shell_cmd!(
                        rt,
                        "{nuget_bin}
                                    install
                                    {non_interactive...}
                                    -ExcludeVersion
                                    -OutputDirectory {install_dir}
                                    -ConfigFile {config_filepath}
                                    {packages_config_filepath}
                                "
                    )
                    .run()?;

                    for (package, package_out_dir) in packages {
                        for var in package_out_dir {
                            rt.write(var, &install_dir.join(&package.id).absolute()?);
                        }
                    }

                    Ok(())
                }
            });
        }

        Ok(())
    }
}
