// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Cmd;
use anyhow::Context;
use cargo_toml::PackageTemplate;
use clap::Parser;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Command {
    /// Use `Cargo.xsync.toml` to regenerate synced `Cargo.toml`
    Regen,
}

#[derive(Parser)]
#[clap(
    about = "Tools to keep Cargo.toml files in-sync",
    disable_help_subcommand = true
)]
pub struct CargoToml {
    #[clap(subcommand)]
    pub cmd: Command,
}

impl Cmd for CargoToml {
    fn run(self, ctx: crate::CmdCtx) -> anyhow::Result<()> {
        let Command::Regen = self.cmd;

        // parse the Cargo.xsync.toml
        let overlay_cargo_toml =
            fs_err::read_to_string(ctx.overlay_workspace.join("Cargo.xsync.toml"))?;
        let mut overlay_cargo_toml = cargo_toml::Manifest::<
            super::custom_meta::CargoOverlayMetadata,
        >::from_slice_with_metadata(
            overlay_cargo_toml.as_bytes()
        )?;

        // extract the custom metadata
        let meta = overlay_cargo_toml
            .workspace
            .as_mut()
            .unwrap()
            .metadata
            .take()
            .unwrap()
            .xsync;

        // rest of the code will extend the overlay toml with inherited fields
        let mut cargo_toml = overlay_cargo_toml;

        // parse the Cargo.toml to sync with
        let base_cargo_toml = fs_err::read_to_string(ctx.base_workspace.join("Cargo.toml"))?;
        let base_cargo_toml =
            cargo_toml::Manifest::<cargo_toml::Value>::from_slice(base_cargo_toml.as_bytes())?;

        //
        // handle simple inherited Cargo.toml fields
        //
        {
            let super::custom_meta::Inherit {
                profile,
                patch,
                workspace: super::custom_meta::InheritWorkspace { lints, package },
                rust_toolchain: _,
            } = meta.inherit;

            if profile {
                cargo_toml.profile = base_cargo_toml.profile.clone();
            }

            if patch {
                cargo_toml.patch = base_cargo_toml.patch.clone();
            }

            if package {
                if cargo_toml.workspace.as_mut().unwrap().package.is_none() {
                    cargo_toml.workspace.as_mut().unwrap().package =
                        Some(PackageTemplate::default());
                }

                (cargo_toml
                    .workspace
                    .as_mut()
                    .unwrap()
                    .package
                    .as_mut()
                    .unwrap())
                .clone_from(
                    base_cargo_toml
                        .workspace
                        .as_ref()
                        .unwrap()
                        .package
                        .as_ref()
                        .unwrap(),
                );
            }

            if lints {
                (cargo_toml.workspace.as_mut().unwrap().lints)
                    .clone_from(&base_cargo_toml.workspace.as_ref().unwrap().lints);
            }
        }

        //
        // handle [workspace.dependencies]
        //
        let inherit_relative_path =
            pathdiff::diff_paths(&ctx.base_workspace, &ctx.overlay_workspace).unwrap();
        for (dep_name, dep) in &mut cargo_toml.workspace.as_mut().unwrap().dependencies {
            match dep {
                cargo_toml::Dependency::Simple(s) if s == "$inherit" => {
                    let mut base_dep = base_cargo_toml
                        .workspace
                        .as_ref()
                        .unwrap()
                        .dependencies
                        .get(dep_name)
                        .with_context(|| {
                            format!(
                                "cannot $inherit {} - dep is not present in base Cargo.toml",
                                dep_name
                            )
                        })?
                        .clone();

                    if let cargo_toml::Dependency::Detailed(details) = &mut base_dep {
                        if let Some(path) = &mut details.path {
                            *path = format!("{}/{path}", inherit_relative_path.display())
                        }
                    }

                    *dep = base_dep;
                }
                _ => {}
            };
        }

        let generated_cargo_toml = format!(
            "{}{}",
            super::GENERATED_HEADER.trim_start(),
            toml_edit::ser::to_string_pretty(&cargo_toml)?
        );

        log::debug!("{generated_cargo_toml}");

        let out = std::path::absolute(ctx.overlay_workspace.join("Cargo.toml"))?;
        if !ctx.check {
            fs_err::write(out, generated_cargo_toml.as_bytes())?;
        } else {
            let existing_cargo_toml = fs_err::read_to_string(&out)?;
            if generated_cargo_toml != existing_cargo_toml {
                anyhow::bail!("{} is out of date!", out.display())
            }
        }

        Ok(())
    }
}
