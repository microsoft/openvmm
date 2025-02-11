// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::Cmd;
use anyhow::Context;
use clap::Parser;
use clap::Subcommand;

#[derive(Subcommand)]
pub enum Command {
    /// Use base repo's `rust-toolchain.toml` to regenerate overlay's `rust-toolchain.toml`
    Regen,
}

#[derive(Parser)]
#[clap(
    about = "Tools to keep rust-toolchain.toml files in-sync",
    disable_help_subcommand = true
)]
pub struct RustToolchainToml {
    #[clap(subcommand)]
    pub cmd: Command,
}

impl Cmd for RustToolchainToml {
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
        let super::custom_meta::InheritRustToolchain {
            inherit,
            channel_prefix,
        } = meta.inherit.rust_toolchain;

        if !inherit {
            return Ok(());
        }

        let out = std::path::absolute(ctx.overlay_workspace.join("rust-toolchain.toml"))?;
        let base_toolchain_toml =
            fs_err::read_to_string(ctx.base_workspace.join("rust-toolchain.toml"));

        // Ensure that the rust-toolchain.toml in the overlay matches that of the base repo exactly,
        // accounting for prefix additions.
        // This is a policy decision, and is open to changing in the future.
        match base_toolchain_toml {
            Ok(base_toolchain_toml) => {
                log::info!(
                    "base rust-toolchain.toml found, regenerating overlay rust-toolchain.toml",
                );
                let mut base_toolchain_toml: schema::RustToolchainToml =
                    toml_edit::de::from_str(&base_toolchain_toml)?;
                if let Some(prefix) = channel_prefix {
                    base_toolchain_toml.toolchain.channel =
                        format!("{}{}", prefix, base_toolchain_toml.toolchain.channel);
                }
                let generated_toolchain_toml = format!(
                    "{}{}",
                    super::GENERATED_HEADER.trim_start(),
                    toml_edit::ser::to_string_pretty(&base_toolchain_toml)?
                );
                log::debug!("{generated_toolchain_toml}");
                fs_err::write(out, generated_toolchain_toml.as_bytes())?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                log::info!("base rust-toolchain.toml not found, removing overlay rust-toolchain.toml if present");
                match fs_err::remove_file(out) {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => Err(e).context("failed to remove overlay rust-toolchain.toml")?,
                }
            }
            Err(e) => {
                Err(e).context("failed to read base rust-toolchain.toml")?;
            }
        }

        Ok(())
    }
}

mod schema {
    use serde::Deserialize;
    use serde::Serialize;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct RustToolchainToml {
        pub toolchain: ToolchainTable,
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct ToolchainTable {
        pub channel: String,
        pub components: Option<Vec<String>>,
        pub targets: Option<Vec<String>>,
        pub profile: Option<String>,
    }
}
