// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build-script helper that emits `BUILD_GIT_SHA` and `BUILD_GIT_BRANCH`
//! cargo environment variables by invoking the `git` CLI.

use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_owned())
}

/// Emit `BUILD_GIT_SHA` and `BUILD_GIT_BRANCH` as `cargo:rustc-env`
/// variables so they are available via `env!()` / `option_env!()` in the
/// consuming crate.
pub fn emit_git_info() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Some(sha) = git_output(&["rev-parse", "HEAD"]) {
        println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");
    }
    if let Some(branch) = git_output(&["rev-parse", "--abbrev-ref", "HEAD"]) {
        println!("cargo:rustc-env=BUILD_GIT_BRANCH={branch}");
    }
}
