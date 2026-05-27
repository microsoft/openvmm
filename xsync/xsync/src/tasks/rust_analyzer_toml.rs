// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Cmd;
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;
use std::str::FromStr;

#[derive(Subcommand)]
pub enum Command {
    /// Use base repo's `rust-analyzer.toml` to regenerate overlay's `rust-analyzer.toml`
    Regen,
}

#[derive(Parser)]
#[clap(
    about = "Tools to keep rust-analyzer.toml files in-sync",
    disable_help_subcommand = true
)]
pub struct RustAnalyzerToml {
    #[clap(subcommand)]
    pub cmd: Command,
}

impl Cmd for RustAnalyzerToml {
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

        if !meta.inherit.rust_analyzer {
            return Ok(());
        }

        let out = std::path::absolute(ctx.overlay_workspace.join("rust-analyzer.toml"))?;
        let base_analyzer_toml =
            fs_err::read_to_string(ctx.base_workspace.join("rust-analyzer.toml"));

        // Ensure that the rust-analyzer.toml in the overlay matches that of the base repo exactly.
        // This is a policy decision, and is open to changing in the future.
        match base_analyzer_toml {
            Ok(base_analyzer_toml) => {
                log::info!(
                    "base rust-analyzer.toml found, regenerating overlay rust-analyzer.toml",
                );
                let mut base_analyzer_toml = toml_edit::DocumentMut::from_str(&base_analyzer_toml)?;
                base_analyzer_toml.fmt();
                let generated_analyzer_toml = format!(
                    "{}{}",
                    super::GENERATED_HEADER.trim_start(),
                    &base_analyzer_toml.to_string()
                );
                log::debug!("{generated_analyzer_toml}");
                fs_err::write(out, generated_analyzer_toml.as_bytes())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!(
                    "base rust-analyzer.toml not found, removing overlay rust-analyzer.toml if present"
                );
                match fs_err::remove_file(out) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => Err(e).context("failed to remove overlay rust-analyzer.toml")?,
                }
            }
            Err(e) => {
                Err(e).context("failed to read base rust-analyzer.toml")?;
            }
        }

        Ok(())
    }
}
