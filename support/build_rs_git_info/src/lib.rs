// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Build-script helper that emits `BUILD_GIT_SHA` and `BUILD_GIT_BRANCH`
//! cargo environment variables by invoking the `git` CLI.

use std::process::Command;

fn git_output(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git").args(args).output()?;

    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed with code {:?}: {}",
            args,
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let output = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    Ok(output)
}

/// Emit `BUILD_GIT_SHA` and `BUILD_GIT_BRANCH` as `cargo:rustc-env`
/// variables so they are available via `env!()` / `option_env!()` in the
/// consuming crate.
pub fn emit_git_info() -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed=.git/HEAD");

    let sha = git_output(&["rev-parse", "HEAD"])?;
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"])?;

    println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=BUILD_GIT_BRANCH={branch}");

    Ok(())
}
