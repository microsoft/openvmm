// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::path::Path;
use std::process::Command;

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
    let stdout = String::from_utf8(output.stdout).ok()?;
    let stdout = stdout.trim().to_owned();
    (!stdout.is_empty()).then_some(stdout)
}

/// The commit this tree was built from, if it is a Git checkout at all.
///
/// A released source archive has no `.git`, and neither does the tree a
/// packager extracts and builds. That is the expected case rather than an
/// error, so this returns `None` instead of failing the build.
fn revision(repo: &Path) -> Option<String> {
    // Git searches parent directories, so an archive extracted inside an
    // unrelated checkout would otherwise silently report *that* checkout's
    // HEAD. Only trust the answer if the repository we found starts exactly
    // where OpenVMM does.
    let toplevel = git(repo, &["rev-parse", "--show-toplevel"])?;
    if std::fs::canonicalize(&toplevel).ok()? != std::fs::canonicalize(repo).ok()? {
        return None;
    }
    git(repo, &["rev-parse", "HEAD"])
}

/// Watch just enough of `.git` to notice HEAD moving.
///
/// This deliberately does not watch tracked files. Reporting whether the tree
/// is dirty would mean emitting a `rerun-if-changed` for every file in the
/// repository, which costs a stat of the whole tree on every single build. The
/// revision on its own is worth two files; a dirty flag is not worth thousands.
fn watch_head(repo: &Path) {
    for path in ["HEAD", "packed-refs"] {
        if let Some(path) = git(repo, &["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={}", repo.join(path).display());
        }
    }
    // On a branch, HEAD is a pointer; the commit changes when the ref it names
    // does, which is a different file.
    if let Some(head_ref) = git(repo, &["symbolic-ref", "HEAD"])
        && let Some(path) = git(repo, &["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={}", repo.join(path).display());
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=OPENVMM_PKGVERSION");

    // The version is a committed fact, inherited from `[workspace.package]`.
    // Git is consulted only to enrich it, never to determine it.
    let product_version = env!("CARGO_PKG_VERSION");

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let revision = revision(&repo_root);
    if revision.is_some() {
        watch_head(&repo_root);
    }

    // `OPENVMM_PKGVERSION` lets whoever builds the binary stamp their own build
    // identity in, the way QEMU's `-Dpkgversion` and cloud-hypervisor's
    // `CH_EXTRA_VERSION` do, so a bug report names the build it came from. An
    // empty value is treated as unset, since build systems routinely pass an
    // undefined variable through as `""`.
    let version = match std::env::var("OPENVMM_PKGVERSION") {
        Ok(pkgversion) if !pkgversion.is_empty() => pkgversion,
        _ => match &revision {
            // Semver build metadata, so it orders identically to the plain
            // version and a build from a checkout is never mistaken for one
            // from the matching release archive.
            Some(revision) => {
                format!("{product_version}+g{}", &revision[..9.min(revision.len())])
            }
            None => product_version.to_owned(),
        },
    };

    println!("cargo:rustc-env=OPENVMM_VERSION={version}");
    println!("cargo:rustc-env=OPENVMM_PRODUCT_VERSION={product_version}");
    println!(
        "cargo:rustc-env=OPENVMM_REVISION={}",
        revision.unwrap_or_default()
    );
}
