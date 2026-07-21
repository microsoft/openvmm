// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![forbid(unsafe_code)]

//! Build-script helper that collects source revision, branch, tag, and dirty
//! state by invoking the `git` CLI.

use std::process::Command;

/// Git source information collected for a build.
#[derive(Debug)]
pub struct GitInfo {
    sha: String,
    branch: String,
    tags: Vec<String>,
    dirty: bool,
    shallow: bool,
}

impl GitInfo {
    /// The full Git commit hash.
    pub fn sha(&self) -> &str {
        &self.sha
    }

    /// The checked-out Git branch, or `HEAD` for a detached checkout.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Tags that point directly at the checked-out commit.
    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    /// Whether the working tree contains tracked or untracked changes.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the checkout is shallow.
    pub fn shallow(&self) -> bool {
        self.shallow
    }

    /// Emit this information as Cargo environment variables.
    pub fn emit(&self) {
        println!("cargo:rustc-env=BUILD_GIT_SHA={}", self.sha);
        println!("cargo:rustc-env=BUILD_GIT_BRANCH={}", self.branch);
        println!("cargo:rustc-env=BUILD_GIT_DIRTY={}", self.dirty);
    }
}

fn git_output(repo: Option<&std::path::Path>, args: &[&str]) -> anyhow::Result<String> {
    let mut command = Command::new("git");
    if let Some(repo) = repo {
        command.arg("-C").arg(repo);
    }
    let output = command.args(args).output()?;

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
    let output = git_output(None, args)?;
    Ok(std::path::absolute(&output)?)
}

fn collect_git_info_inner(repo: Option<&std::path::Path>) -> anyhow::Result<GitInfo> {
    // Always rerun when HEAD changes (e.g. branch switch).
    let head_path = match repo {
        Some(repo) => {
            let output = git_output(Some(repo), &["rev-parse", "--git-path", "HEAD"])?;
            std::path::absolute(repo.join(output))?
        }
        None => git_path(&["rev-parse", "--git-path", "HEAD"])?,
    };
    println!("cargo:rerun-if-changed={}", head_path.display());

    for git_path in ["refs/tags", "packed-refs"] {
        let output = git_output(repo, &["rev-parse", "--git-path", git_path])?;
        let path = match repo {
            Some(repo) => std::path::absolute(repo.join(output))?,
            None => std::path::absolute(output)?,
        };
        println!("cargo:rerun-if-changed={}", path.display());
    }

    // If HEAD is a symbolic ref (i.e. points at a branch), also watch the
    // branch ref file so we rebuild when new commits land on that branch.
    if let Ok(head_ref) = git_output(repo, &["symbolic-ref", "HEAD"]) {
        // e.g. refs/heads/main → .git/refs/heads/main (or the worktree equivalent)
        let output = git_output(repo, &["rev-parse", "--git-path", &head_ref])?;
        let ref_path = match repo {
            Some(repo) => std::path::absolute(repo.join(output))?,
            None => std::path::absolute(output)?,
        };
        println!("cargo:rerun-if-changed={}", ref_path.display());
    }

    let sha = git_output(repo, &["rev-parse", "HEAD"])?;
    let branch = git_output(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let tags = git_output(repo, &["tag", "--points-at", "HEAD"])?
        .lines()
        .map(str::to_owned)
        .collect();
    // Cargo cannot practically watch every repository file, so this is
    // refreshed whenever another watched build input changes.
    let dirty =
        !git_output(repo, &["status", "--porcelain", "--untracked-files=normal"])?.is_empty();
    let shallow = git_output(repo, &["rev-parse", "--is-shallow-repository"])? == "true";

    Ok(GitInfo {
        sha,
        branch,
        tags,
        dirty,
        shallow,
    })
}

/// Collect Git information for the current checkout.
pub fn collect_git_info() -> anyhow::Result<GitInfo> {
    collect_git_info_inner(None)
}

/// Collect Git information from an expected repository root.
///
/// This rejects a parent repository so vendored source does not accidentally
/// inherit the identity of the repository that contains it.
pub fn collect_git_info_at(repo: &std::path::Path) -> anyhow::Result<GitInfo> {
    let expected_root = std::fs::canonicalize(repo)?;
    let actual_root = git_output(Some(repo), &["rev-parse", "--show-toplevel"])?;
    let actual_root = std::fs::canonicalize(actual_root)?;
    anyhow::ensure!(
        actual_root == expected_root,
        "{} is not the Git repository root",
        repo.display()
    );
    collect_git_info_inner(Some(repo))
}

/// Emit git information as `cargo:rustc-env` variables so they are available via
/// `env!()` / `option_env!()` in the consuming crate.
pub fn emit_git_info() -> anyhow::Result<()> {
    collect_git_info()?.emit();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn run(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn temporary_repo() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo =
            std::env::temp_dir().join(format!("build-rs-git-info-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "--quiet"]);
        run(&repo, &["config", "core.autocrlf", "false"]);
        run(&repo, &["config", "core.safecrlf", "false"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "Build Test"]);
        std::fs::write(repo.join("tracked.txt"), "tracked\n").unwrap();
        run(&repo, &["add", "tracked.txt"]);
        run(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    #[test]
    fn collects_exact_tags_and_dirty_state() {
        let repo = temporary_repo();
        run(&repo, &["tag", "openvmm-v0.1.0"]);

        let clean = collect_git_info_at(&repo).unwrap();
        assert_eq!(clean.tags(), ["openvmm-v0.1.0"]);
        assert!(!clean.dirty());
        assert!(!clean.shallow());

        std::fs::write(repo.join("untracked.txt"), "dirty\n").unwrap();
        assert!(collect_git_info_at(&repo).unwrap().dirty());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn rejects_a_directory_inside_a_parent_repository() {
        let repo = temporary_repo();
        let nested = repo.join("vendored-openvmm");
        std::fs::create_dir(&nested).unwrap();

        let error = collect_git_info_at(&nested).unwrap_err();
        assert!(error.to_string().contains("is not the Git repository root"));

        std::fs::remove_dir_all(repo).unwrap();
    }
}
