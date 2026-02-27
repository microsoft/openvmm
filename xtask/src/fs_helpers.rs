// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helper functions to traverse + enumerate the project's filesystem, used by
//! multiple task implementations.

use std::collections::BTreeSet;
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

/// Return files tracked by git (excluding those from .gitignore), including
/// those which have not yet been staged / committed.
pub fn git_ls_files() -> anyhow::Result<Vec<PathBuf>> {
    let sh = XtaskShell::new()?;

    macro_rules! as_set {
        ($($arg:literal),+) => {{
            let output = sh.cmd("git").args([$($arg),+]).output()?.stdout;
            let output = String::from_utf8_lossy(&output).to_string();
            output
                .split('\n')
                .map(PathBuf::from)
                .collect::<BTreeSet<_>>()
        }};
    }

    // "extra" corresponds to files not-yet committed to git
    let all = as_set!("ls-files");
    let extra = as_set!("ls-files", "--others", "--exclude-standard");
    let deleted = as_set!("ls-files", "--deleted");

    let mut allow_list = all;
    allow_list.extend(extra);
    allow_list = allow_list.difference(&deleted).cloned().collect();

    // Vec is returned in sorted order because of BTreeSet iteration order
    Ok(allow_list.into_iter().collect())
}
