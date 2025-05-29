// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub use flowey::util::copy_dir_all;

use flowey::node::prelude::FlowPlatformKind;
use flowey::node::prelude::RustRuntimeServices;

pub mod cargo_output;
pub mod extract;
pub mod wslpath;

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
