// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::PathBuf;

pub fn win_to_linux(path: impl AsRef<std::path::Path>) -> PathBuf {
    let sh = xshell::Shell::new().unwrap();
    let path = path.as_ref();
    xshell::cmd!(sh, "wslpath {path}")
        .quiet()
        .ignore_status()
        .read()
        .unwrap()
        .into()
}

pub fn linux_to_win(path: impl AsRef<std::path::Path>) -> PathBuf {
    let sh = xshell::Shell::new().unwrap();
    let path = path.as_ref();
    xshell::cmd!(sh, "wslpath -aw {path}")
        .quiet()
        .ignore_status()
        .read()
        .unwrap()
        .into()
}
