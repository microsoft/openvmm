// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use anyhow::Context;
use anyhow::anyhow;
use std::ffi::OsStr;
use std::path::Path;

pub fn check_crate_name_nodash(path: &Path) -> anyhow::Result<()> {
    // while it's _theoretically_ possible to support --fix here, adding new
    // crates is a relatively uncommon operation, so it's probably not worth the
    // effort of automating it...

    if path.file_name() != Some(OsStr::new("Cargo.toml")) {
        return Ok(());
    }

    let contents = fs_err::read_to_string(path)?;
    let parsed = contents.parse::<toml_edit::DocumentMut>()?;

    let package_name = match parsed
        .as_table()
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        Some(name) => name,
        None => return Ok(()), // Workspace root toml
    };

    if let Some(metadata) = parsed
        .get("package")
        .and_then(|x| x.get("metadata"))
        .and_then(|x| x.get("xtask"))
        .and_then(|x| x.get("house-rules"))
    {
        let props = metadata.as_table().context("invalid metadata format")?;
        for (k, v) in props.iter() {
            if k == "allow-dash-in-name" {
                let is_bin = v
                    .as_bool()
                    .context("invalid type for allow-dash-in-name (must be bool)")?;
                if is_bin {
                    return Ok(());
                }
            }
        }
    }

    let bad_package_name = package_name.contains('-');
    let bad_package_path = {
        if let Some(parent_path) = path.parent() {
            parent_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .contains('-')
        } else {
            false
        }
    };

    let msg = match (bad_package_name, bad_package_path) {
        (true, true) => "crate name + folder cannot contain '-' char",
        (true, false) => "crate name cannot contain '-' char",
        (false, true) => "crate folder cannot contain '-' char",
        _ => return Ok(()),
    };

    Err(anyhow!(
        "{}: name={} folder={}",
        msg,
        package_name,
        path.parent().unwrap_or_else(|| Path::new("")).display()
    ))
}
