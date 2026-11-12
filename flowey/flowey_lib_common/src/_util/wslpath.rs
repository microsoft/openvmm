// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::Context;
use flowey::node::prelude::RustRuntimeServices;
use flowey::node::prelude::anyhow;
use std::path::Path;
use std::path::PathBuf;

fn convert(
    rt: &RustRuntimeServices<'_>,
    path: &Path,
    absolute_windows_path: bool,
) -> anyhow::Result<PathBuf> {
    let converted = if absolute_windows_path {
        flowey::shell_cmd!(rt, "wslpath -aw {path}")
            .quiet()
            .read()?
    } else {
        flowey::shell_cmd!(rt, "wslpath {path}").quiet().read()?
    };

    anyhow::ensure!(
        !converted.trim().is_empty(),
        "wslpath returned an empty path for {}",
        path.display()
    );

    Ok(converted.trim().into())
}

pub fn win_to_linux(
    rt: &RustRuntimeServices<'_>,
    path: impl AsRef<Path>,
) -> anyhow::Result<PathBuf> {
    let path = path.as_ref();
    convert(rt, path, false)
        .with_context(|| format!("failed to convert Windows path {}", path.display()))
}

pub fn linux_to_win(
    rt: &RustRuntimeServices<'_>,
    path: impl AsRef<Path>,
) -> anyhow::Result<PathBuf> {
    let path = path.as_ref();
    convert(rt, path, true)
        .with_context(|| format!("failed to convert Linux path {}", path.display()))
}
