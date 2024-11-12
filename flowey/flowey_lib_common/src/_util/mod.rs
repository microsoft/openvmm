// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::FlowPlatformKind;
use flowey::node::prelude::RustRuntimeServices;
use std::path::Path;

pub mod extract;
pub mod wslpath;

pub fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> std::io::Result<()> {
    fs_err::create_dir_all(&dst)?;
    for entry in fs_err::read_dir(src.as_ref())? {
        let entry = entry?;
        let dst = dst.as_ref().join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(entry.path(), dst)?;
        } else {
            fs_err::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}

// include a "dummy" _rt argument to enforce that this helper should only be
// used in runtime contexts, and not during flow compile-time.
pub fn running_in_wsl(_rt: &mut RustRuntimeServices<'_>) -> bool {
    let Ok(output) = std::process::Command::new("wslpath")
        .args(["-aw", "/"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout).starts_with(r"\\wsl.localhost")
}

/// Returns the name of the bsdtar binary to use. On Windows, this is just the
/// inbox tar.exe. Elsewhere, use bsdtar. This will require installing the
/// libarchive-tools package on Debian-based Linux.
pub fn bsdtar_name(rt: &mut RustRuntimeServices<'_>) -> &'static str {
    match rt.platform().kind() {
        FlowPlatformKind::Windows => "tar.exe",
        FlowPlatformKind::Unix => "bsdtar",
    }
}
