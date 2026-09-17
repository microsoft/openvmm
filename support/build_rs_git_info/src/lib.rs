// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

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

fn git_path(args: &[&str]) -> anyhow::Result<std::path::PathBuf> {
    let output = git_output(args)?;
    Ok(std::path::absolute(&output)?)
}

/// Emit git information as `cargo:rustc-env` variables so they are available via
/// `env!()` / `option_env!()` in the consuming crate.
pub fn emit_git_info() -> anyhow::Result<()> {
    // Always rerun when HEAD changes (e.g. branch switch).
    let head_path = git_path(&["rev-parse", "--git-path", "HEAD"])?;
    println!("cargo:rerun-if-changed={}", head_path.display());

    // If HEAD is a symbolic ref (i.e. points at a branch), also watch the
    // branch ref file so we rebuild when new commits land on that branch.
    if let Ok(head_ref) = git_output(&["symbolic-ref", "HEAD"]) {
        // e.g. refs/heads/main → .git/refs/heads/main (or the worktree equivalent)
        let ref_path = git_path(&["rev-parse", "--git-path", &head_ref])?;
        println!("cargo:rerun-if-changed={}", ref_path.display());
    }

    let sha = git_output(&["rev-parse", "HEAD"])?;
    let branch = git_output(&["rev-parse", "--abbrev-ref", "HEAD"])?;

    println!("cargo:rustc-env=BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=BUILD_GIT_BRANCH={branch}");

    Ok(())
}
