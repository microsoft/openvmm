// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use vergen_gitcl::Emitter;
use vergen_gitcl::Gitcl;

#[path = "src/version.rs"]
mod version;

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|output| output.trim().to_owned())
}

fn watch_git_path(repo: &Path, name: &str) {
    if let Some(path) = git(repo, &["rev-parse", "--git-path", name]) {
        let path = PathBuf::from(path);
        let path = if path.is_absolute() {
            path
        } else {
            repo.join(path)
        };
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn collect_git_source(repo: &Path) -> Option<version::GitSource> {
    // Do not let an extracted archive nested under an unrelated checkout
    // inherit that checkout's identity.
    if !repo.join(".git").exists() {
        return None;
    }

    watch_git_path(repo, "HEAD");
    watch_git_path(repo, "packed-refs");
    if let Some(head_ref) = git(repo, &["symbolic-ref", "HEAD"]) {
        watch_git_path(repo, &head_ref);
    }
    // Refresh the dirty marker when changes are staged. Unstaged-only changes
    // are intentionally best-effort until another watched input changes.
    watch_git_path(repo, "index");

    let mut git = Gitcl::builder().sha(false).dirty(false).build();
    git.at_path(repo.to_owned());
    let mut emitter = Emitter::default();
    emitter.add_instructions(&git).ok()?;
    let values = emitter.cargo_rustc_env_map();
    let value = |name| {
        values
            .iter()
            .find_map(|(key, value)| (key.name() == name).then(|| value.clone()))
    };

    let revision = value("VERGEN_GIT_SHA")?;
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        panic!("git returned an invalid OpenVMM revision: {revision:?}");
    }

    let dirty = value("VERGEN_GIT_DIRTY")?.parse().ok()?;

    Some(version::GitSource { revision, dirty })
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/version.rs");

    let product_version = env!("CARGO_PKG_VERSION");
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = collect_git_source(&repo_root);

    let version = version::resolve_version(product_version, git);
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let revision = if version.revision.is_empty() {
        "(not built from a checkout)"
    } else {
        &version.revision
    };
    let long_version = format!(
        "{}\n\
         build:   {}\n\
         version: {product_version}\n\
         commit:  {revision}\n\
         host:    {target}",
        version.version,
        version.kind.description(),
    );

    println!("cargo:rustc-env=OPENVMM_VERSION={}", version.version);

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out_dir.join("long_version.txt"), long_version)
        .expect("failed to write long version");
}
