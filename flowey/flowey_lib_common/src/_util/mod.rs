// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub use flowey::util::copy_dir_all;

use flowey::node::prelude::FlowPlatformKind;
use flowey::node::prelude::RustRuntimeServices;
use flowey::node::prelude::anyhow;
use std::collections::BTreeMap;

pub mod cargo_install;
pub mod cargo_output;
pub mod extract;
pub mod wslpath;

/// Group values by key, ordering groups deterministically and retaining all
/// values within each group in input order.
pub fn group_by<K: Ord, V>(items: impl IntoIterator<Item = (K, V)>) -> BTreeMap<K, Vec<V>> {
    let mut groups = BTreeMap::<_, Vec<_>>::new();
    for (key, value) in items {
        groups.entry(key).or_default().push(value);
    }
    groups
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

/// number of times to retry github operations
pub const GH_RETRIES: usize = 5;

/// Helper for retrying operations
pub fn retry<T>(
    rt: &mut RustRuntimeServices<'_>,
    f: fn(&mut RustRuntimeServices<'_>, &T) -> anyhow::Result<()>,
    args: &T,
    attempts: usize,
) -> anyhow::Result<()> {
    let mut i = 0;
    while let Err(e) = f(rt, args) {
        i += 1;
        if i == attempts {
            return Err(e.into());
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    Ok(())
}
