// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Prefix of the Git tag naming an OpenVMM release.
///
/// This is intentionally duplicated from the release tooling. Source consumers
/// build this crate without that tooling, so build identity cannot depend on it.
const RELEASE_TAG_PREFIX: &str = "openvmm-v";

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

fn git_repository_starts_at(repo: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--show-prefix"])
        .output()
        .is_ok_and(|output| {
            output.status.success() && output.stdout.iter().all(u8::is_ascii_whitespace)
        })
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
    if !git_repository_starts_at(repo) {
        return None;
    }
    git(repo, &["rev-parse", "HEAD"])
}

/// Whether HEAD is the exact commit tagged for `version`.
///
/// A checkout without tags answers "no", which fails safely: it may report a
/// release checkout as a development build, but never the reverse.
fn at_release_tag(repo: &Path, version: &str) -> bool {
    let Some(tags) = git(repo, &["tag", "--points-at", "HEAD"]) else {
        return false;
    };
    let release_tag = format!("{RELEASE_TAG_PREFIX}{version}");
    tags.lines().any(|tag| tag.trim() == release_tag)
}

/// Notice the release tag arriving after the tree was already built.
fn watch_release_tag(repo: &Path, version: &str) {
    let tag = format!("refs/tags/{RELEASE_TAG_PREFIX}{version}");
    if let Some(path) = git(repo, &["rev-parse", "--git-path", &tag]) {
        println!("cargo:rerun-if-changed={}", repo.join(path).display());
    }
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
        watch_release_tag(&repo_root, product_version);
    }
    let at_release_tag = revision.is_some() && at_release_tag(&repo_root, product_version);

    // `OPENVMM_PKGVERSION` lets whoever builds the binary stamp their own build
    // identity in, the way QEMU's `-Dpkgversion` and cloud-hypervisor's
    // `CH_EXTRA_VERSION` do, so a bug report names the build it came from. An
    // empty value is treated as unset, since build systems routinely pass an
    // undefined variable through as `""`.
    let pkgversion = match std::env::var("OPENVMM_PKGVERSION") {
        Ok(pkgversion) if !pkgversion.is_empty() => Some(pkgversion),
        _ => None,
    };

    let version = match &pkgversion {
        Some(pkgversion) => pkgversion.clone(),
        None => match &revision {
            // Semver build metadata, so it orders identically to the plain
            // version and a build from a checkout is never mistaken for one
            // from the matching release archive.
            Some(revision) if !at_release_tag => {
                format!("{product_version}+g{}", &revision[..9.min(revision.len())])
            }
            _ => product_version.to_owned(),
        },
    };

    // Deliberately not a boolean "official". OpenVMM ships as source that
    // someone else builds, so a packager's binary is legitimately not ours and
    // yet is a legitimate build of an official version. Report what is known
    // and let the consumer judge.
    let (kind, kind_description) = if pkgversion.is_some() {
        ("custom", "custom (built with OPENVMM_PKGVERSION)")
    } else if product_version.contains('-') {
        // A semver prerelease component. `main` carries `-dev`, so anything
        // built from it says so.
        ("development", "development (not an official release)")
    } else {
        ("release", "release")
    };

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".into());
    let long_version = format!(
        "{version}\n\
         build:   {kind_description}\n\
         version: {product_version}\n\
         commit:  {}\n\
         host:    {target}",
        revision.as_deref().unwrap_or("(not built from a checkout)"),
    );

    println!("cargo:rustc-env=OPENVMM_VERSION={version}");
    println!("cargo:rustc-env=OPENVMM_PRODUCT_VERSION={product_version}");
    println!("cargo:rustc-env=OPENVMM_BUILD_KIND={kind}");
    println!("cargo:rustc-env=OPENVMM_TARGET={target}");
    // Written to a file rather than emitted as `rustc-env`, because cargo parses
    // build script output a line at a time and would silently keep only the
    // first line of a multi-line value. Pre-formatted here because a build
    // script can compose it from optional parts, where `concat!` in the library
    // could not.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(out_dir.join("long_version.txt"), &long_version)
        .expect("failed to write long version");
    println!(
        "cargo:rustc-env=OPENVMM_REVISION={}",
        revision.unwrap_or_default()
    );
}
