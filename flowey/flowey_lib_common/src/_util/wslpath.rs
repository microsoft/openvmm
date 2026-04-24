// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use flowey::node::prelude::RustRuntimeServices;
use std::path::Path;
use std::path::PathBuf;

pub fn win_to_linux(rt: &RustRuntimeServices<'_>, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    flowey::shell_cmd!(rt, "wslpath {path}")
        .quiet()
        .ignore_status()
        .read()
        .unwrap()
        .into()
}

pub fn linux_to_win(rt: &RustRuntimeServices<'_>, path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    flowey::shell_cmd!(rt, "wslpath -aw {path}")
        .quiet()
        .ignore_status()
        .read()
        .unwrap()
        .into()
}
