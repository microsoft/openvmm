// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper functions to traverse + enumerate the project's filesystem, used by
//! multiple task implementations.

use std::path::PathBuf;

use crate::shell::XtaskShell;

/// Return a list of all files that are currently git diffed, including
/// those which have been staged, but not yet been committed.
pub fn git_diffed(in_git_hook: bool) -> anyhow::Result<Vec<PathBuf>> {
    let sh = XtaskShell::new()?;

    let files = sh
        .cmd("git")
        .args(["diff", "--diff-filter", "MAR", "--name-only"])
        .output()?
        .stdout;
    let files_cached = sh
        .cmd("git")
        .args(["diff", "--diff-filter", "MAR", "--name-only", "--cached"])
        .output()?
        .stdout;

    let files = String::from_utf8_lossy(&files);
    let files_cached = String::from_utf8_lossy(&files_cached);

    // don't include unstaged files when running in a hook context
    let files: Box<dyn Iterator<Item = _>> = if in_git_hook {
        Box::new(files_cached.lines())
    } else {
        Box::new(files_cached.lines().chain(files.lines()))
    };

    let mut all_files = files.map(PathBuf::from).collect::<Vec<_>>();

    all_files.sort();
    all_files.dedup();
    Ok(all_files)
}
