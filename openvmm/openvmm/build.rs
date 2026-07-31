// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use std::path::Path;
use std::process::Command;

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

fn at_release_tag(repo: &Path, version: &str) -> bool {
    let Some(tags) = git(repo, &["tag", "--points-at", "HEAD"]) else {
        return false;
    };
    let release_tag = format!("{RELEASE_TAG_PREFIX}{version}");
    tags.lines().any(|tag| tag.trim() == release_tag)
}

fn watch_git_identity(repo: &Path, version: &str) {
    for path in ["HEAD", "packed-refs"] {
        if let Some(path) = git(repo, &["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={}", repo.join(path).display());
        }
    }
    if let Some(head_ref) = git(repo, &["symbolic-ref", "HEAD"])
        && let Some(path) = git(repo, &["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={}", repo.join(path).display());
    }
    let tag = format!("refs/tags/{RELEASE_TAG_PREFIX}{version}");
    if let Some(path) = git(repo, &["rev-parse", "--git-path", &tag]) {
        println!("cargo:rerun-if-changed={}", repo.join(path).display());
    }
}

fn main() {
    // Prevent this build script from rerunning unnecessarily.
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        println!("cargo:rustc-link-lib=onecore_apiset");
        println!("cargo:rustc-link-lib=onecoreuap_apiset");

        // Embed version/resource info into the EXE.
        println!("cargo:rerun-if-changed=resources.rc");
        println!("cargo:rerun-if-env-changed=OPENVMM_MAJOR");
        println!("cargo:rerun-if-env-changed=OPENVMM_MINOR");
        println!("cargo:rerun-if-env-changed=OPENVMM_PATCH");
        println!("cargo:rerun-if-env-changed=OPENVMM_REVISION");

        // Default to the crate version so that the version Windows reports in
        // the file properties and the one `openvmm --version` prints cannot
        // disagree. The `OPENVMM_*` vars still win, which is how a build
        // pipeline stamps its own build number in. There is no crate
        // equivalent of the fourth component, so it stays 0 unless set.
        let parse_u16 = |s: String| s.parse::<u16>().unwrap_or(0);
        let component = |var: &str, from_crate_version: &str| {
            std::env::var(var)
                .or_else(|_| std::env::var(from_crate_version))
                .map(parse_u16)
                .unwrap_or(0)
        };
        let major = component("OPENVMM_MAJOR", "CARGO_PKG_VERSION_MAJOR");
        let minor = component("OPENVMM_MINOR", "CARGO_PKG_VERSION_MINOR");
        let patch = component("OPENVMM_PATCH", "CARGO_PKG_VERSION_PATCH");
        let revision = std::env::var("OPENVMM_REVISION")
            .map(parse_u16)
            .unwrap_or(0);

        // VS_FF_PRERELEASE. Keep Windows file metadata consistent with
        // `openvmm --version`: any checkout other than the exact release tag is
        // a development build, while an extracted source archive is a release.
        // A build script cannot read another crate's `rustc-env`, so this small
        // Git probe is intentionally duplicated from `openvmm_build_info`.
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let checkout = git_repository_starts_at(&repo_root);
        if checkout {
            watch_git_identity(&repo_root, env!("CARGO_PKG_VERSION"));
        }
        let prerelease = checkout && !at_release_tag(&repo_root, env!("CARGO_PKG_VERSION"));
        let file_flags = if prerelease { 0x2 } else { 0x0 };

        let macros = [
            (
                "OPENVMM_VERSION",
                format!("{major},{minor},{patch},{revision}"),
            ),
            (
                "OPENVMM_VERSION_STR",
                format!(r#""{major}.{minor}.{patch}.{revision}""#),
            ),
            ("OPENVMM_FILE_FLAGS", format!("{file_flags:#x}")),
        ];

        embed_resource::compile(
            "resources.rc",
            macros.iter().map(|(k, v)| format!("{k}={v}")),
        )
        .manifest_required()
        .expect("Failed to embed resources");
    }
}
