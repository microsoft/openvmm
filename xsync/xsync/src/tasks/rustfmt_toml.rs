// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Cmd;
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use std::str::FromStr;

#[derive(Subcommand)]
pub enum Command {
    /// Use base repo's `rustfmt.toml` to regenerate overlay's `rustfmt.toml`
    Regen,
}

#[derive(Parser)]
#[clap(
    about = "Tools to keep rustfmt.toml files in-sync",
    disable_help_subcommand = true
)]
pub struct RustfmtToml {
    #[clap(subcommand)]
    pub cmd: Command,
}

impl Cmd for RustfmtToml {
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

        if !meta.inherit.rustfmt {
            return Ok(());
        }

        let out = std::path::absolute(ctx.overlay_workspace.join("rustfmt.toml"))?;
        let base_fmt_toml = fs_err::read_to_string(ctx.base_workspace.join("rustfmt.toml"));

        // Ensure that the rustfmt.toml in the overlay matches that of the base repo exactly.
        // This is a policy decision, and is open to changing in the future.
        match base_fmt_toml {
            Ok(base_fmt_toml) => {
                log::info!("base rustfmt.toml found, regenerating overlay rustfmt.toml",);
                let mut base_fmt_toml = toml_edit::Document::from_str(&base_fmt_toml)?;
                base_fmt_toml.fmt();
                let generated_fmt_toml = format!(
                    "{}{}",
                    super::GENERATED_HEADER.trim_start(),
                    &base_fmt_toml.to_string()
                );
                log::debug!("{generated_fmt_toml}");
                fs_err::write(out, generated_fmt_toml.as_bytes())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("base rustfmt.toml not found, removing overlay rustfmt.toml if present");
                match fs_err::remove_file(out) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => Err(e).context("failed to remove overlay rustfmt.toml")?,
                }
            }
            Err(e) => {
                Err(e).context("failed to read base rustfmt.toml")?;
            }
        }

        Ok(())
    }
}
