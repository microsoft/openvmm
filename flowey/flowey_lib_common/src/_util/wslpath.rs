// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::path::Path;
use std::path::PathBuf;

#[expect(clippy::disallowed_methods, clippy::disallowed_macros)]
pub fn win_to_linux(path: impl AsRef<Path>) -> PathBuf {
    let sh = xshell::Shell::new().unwrap();
    let path = path.as_ref();
    xshell::cmd!(sh, "wslpath {path}")
        .quiet()
        .ignore_status()
        .read()
        .unwrap()
        .into()
}

#[expect(clippy::disallowed_methods, clippy::disallowed_macros)]
pub fn linux_to_win(path: impl AsRef<Path>) -> PathBuf {
    let sh = xshell::Shell::new().unwrap();
    let path = path.as_ref();
    xshell::cmd!(sh, "wslpath -aw {path}")
        .quiet()
        .ignore_status()
        .read()
        .unwrap()
        .into()
}
