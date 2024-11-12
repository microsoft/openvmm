// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Init new `fuzz/` directories using a preset template.

use super::cargo_package_metadata;
use anyhow::Context;

#[derive(Clone, clap::ValueEnum)]
pub enum Template {
    /// Quick and easy `ChipsetDevice` fuzzing
    ChipsetDevice,
    /// Basic `cargo-fuzz` template
    Basic,
}

pub(super) fn init_from_template(
    ctx: &crate::XtaskCtx,
    package: String,
    template: Template,
) -> Result<(), anyhow::Error> {
    let selected_crate = 'selected_crate: {
        let cargo_tomls = ignore::Walk::new(&ctx.root).filter_map(|entry| match entry {
            Ok(entry) if entry.file_name() == "Cargo.toml" => Some(entry.into_path()),
            Err(err) => {
                log::error!("error when walking over subdirectories: {}", err);
                None
            }
            _ => None,
        });

        for cargo_toml_path in cargo_tomls {
            let manifest = cargo_toml::Manifest::<cargo_package_metadata::PackageMetadata>::from_path_with_metadata(
                &cargo_toml_path,
            )?;

            if manifest
                .package
                .as_ref()
                .map(|x| x.name())
                .unwrap_or_default()
                == package
            {
                break 'selected_crate cargo_toml_path;
            }
        }

        anyhow::bail!("unknown crate '{}'", package)
    };

    let new_fuzz_target = format!("fuzz_{}", package);
    let new_fuzz_dir = selected_crate.parent().unwrap().join("fuzz");

    // make the new fuzz/ dir with the templated files
    if new_fuzz_dir.exists() {
        anyhow::bail!("{} already has a `fuzz/` folder!", package)
    }
    fs_err::create_dir(&new_fuzz_dir)?;
    fs_err::write(
        new_fuzz_dir.join("Cargo.toml"),
        match template {
            Template::Basic => include_str!("./templates/basic.template.toml"),
            Template::ChipsetDevice => include_str!("./templates/chipset_device.template.toml"),
        }
        .to_string()
        .replacen("$FUZZ_CRATE_NAME$", &new_fuzz_target, usize::MAX)
        .replacen("$PARENT$", &package, usize::MAX)
        .replacen("$FUZZ_TARGET_NAME$", &new_fuzz_target, usize::MAX),
    )?;
    fs_err::write(
        new_fuzz_dir.join(format!("{}.rs", new_fuzz_target)),
        match template {
            Template::Basic => include_str!("./templates/basic.template.rs"),
            Template::ChipsetDevice => include_str!("./templates/chipset_device.template.rs"),
        },
    )?;

    // also update the root workspace toml
    let new_workspace_entry = new_fuzz_dir.strip_prefix(&ctx.root)?.display().to_string();
    let root_toml_raw = fs_err::read_to_string(ctx.root.join("Cargo.toml"))?;
    let mut root_toml = root_toml_raw
        .parse::<toml_edit::Document>()
        .context("invalid root workspace Cargo.toml")?;
    let members = &mut root_toml["workspace"]["members"].as_array_mut().unwrap();
    // TODO: slot the new fuzz crate into the workspace members array in *sorted order*
    //       (as opposed to appending blindly to the end of the array, as we do today)
    members.push_formatted(toml_edit::Value::from(new_workspace_entry).decorated("\n  ", ""));
    fs_err::write(ctx.root.join("Cargo.toml"), root_toml.to_string())?;

    Ok(())
}
