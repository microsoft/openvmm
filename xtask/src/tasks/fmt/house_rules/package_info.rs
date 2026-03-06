// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Checks to ensure that the `[package]` sections of Cargo.toml files do not
//! contain `authors` or `version` fields, and that rust-version is properly
//! workspace.
//!
//! Eliding the [version][] sets the version to "0.0.0", which is fine. More
//! importantly, it means the module cannot be published to crates.io
//! (equivalent to publish = false), which is what we want for our internal
//! crates. And removing the meaningless version also eliminates more questions
//! from newcomers (does the version field mean anything? do we use it for
//! semver internally?).
//!
//! The [authors][] field is optional, is not really used anywhere anymore, and
//! just creates confusion.
//!
//! [version]:
//!     <https://doc.rust-lang.org/cargo/reference/manifest.html#the-version-field>
//! [authors]:
//!     <https://doc.rust-lang.org/cargo/reference/manifest.html#the-authors-field>

use anyhow::Context;
use std::ffi::OsStr;
use std::path::Path;
use toml_edit::Item;
use toml_edit::Table;

/// List of packages that are allowed to have a version
static VERSION_EXCEPTIONS: &[&str] = &["vmgstool"];

pub fn check_package_info(f: &Path, fix: bool) -> anyhow::Result<()> {
    if f.file_name() != Some(OsStr::new("Cargo.toml")) {
        return Ok(());
    }

    let contents = fs_err::read_to_string(f)?;
    let mut parsed = contents.parse::<toml_edit::DocumentMut>()?;

    let mut excluded_from_workspace = false;
    if let Some(metadata) = parsed
        .get("package")
        .and_then(|x| x.get("metadata"))
        .and_then(|x| x.get("xtask"))
        .and_then(|x| x.get("house-rules"))
    {
        let props = metadata.as_table().context("invalid metadata format")?;
        for (k, v) in props.iter() {
            if k == "excluded-from-workspace" {
                excluded_from_workspace = v
                    .as_bool()
                    .context("invalid type for excluded-from-workspace (must be bool)")?;
            }
        }
    }

    if !parsed.contains_key("package") {
        // workspace root, skip
        return Ok(());
    }

    let mut lints_table = Table::new();
    lints_table.insert("workspace", Item::Value(true.into()));
    let old_lints_table = parsed.insert("lints", Item::Table(lints_table.clone()));

    let package = parsed
        .get_mut("package")
        .with_context(|| format!("missing package section in {}", f.display()))?
        .as_table_mut()
        .with_context(|| format!("invalid package section in {}", f.display()))?;

    let mut rust_version_field = Table::new();
    rust_version_field.set_dotted(true);
    rust_version_field.insert("workspace", Item::Value(true.into()));
    let old_rust_version = package.insert("rust-version", Item::Table(rust_version_field.clone()));

    let mut edition_field = Table::new();
    edition_field.set_dotted(true);
    edition_field.insert("workspace", Item::Value(true.into()));
    let old_edition_field = package.insert("edition", Item::Table(edition_field.clone()));

    let package_name = package
        .get("name")
        .with_context(|| format!("missing package name in {}", f.display()))?
        .as_str()
        .with_context(|| format!("invalid package name in {}", f.display()))?;
    let check_version = !VERSION_EXCEPTIONS.contains(&package_name);

    // Note careful use of non-short-circuiting or.
    let invalid = package.remove("authors").is_some()
        | check_version
            .then(|| package.remove("version"))
            .flatten()
            .is_some()
        | (!excluded_from_workspace
            && (old_lints_table.map(|o| o.to_string()) != Some(lints_table.to_string()))
                | (old_rust_version.map(|o| o.to_string())
                    != Some(rust_version_field.to_string()))
                | (old_edition_field.map(|o| o.to_string()) != Some(edition_field.to_string())));

    if invalid {
        if !fix {
            anyhow::bail!(
                "invalid inclusion of package authors or version, or non-workspaced lints, rust-version, or edition, in {}",
                f.display()
            );
        }
        fs_err::write(f, parsed.to_string())?;
        log::info!("fixed package section in {}", f.display());
    }

    Ok(())
}
