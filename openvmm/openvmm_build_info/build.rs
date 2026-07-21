// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use serde::Deserialize;

mod version;

const RELEASE_METADATA_SCHEMA: u32 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SerializedReleaseMetadata {
    schema_version: u32,
    version: String,
    tag: String,
    revision: String,
}

fn read_release_metadata(path: &std::path::Path) -> Option<version::ReleaseMetadata> {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("failed to read {}: {error}", path.display()),
    };
    let metadata: SerializedReleaseMetadata = serde_json::from_slice(&contents)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
    assert_eq!(
        metadata.schema_version, RELEASE_METADATA_SCHEMA,
        "unsupported OpenVMM release metadata schema"
    );
    let metadata = version::ReleaseMetadata {
        version: metadata.version,
        tag: metadata.tag,
        revision: metadata.revision,
    };
    version::validate_release_metadata(&metadata)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", path.display()));
    Some(metadata)
}

fn git_output(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn watch_git_inputs(repo: &std::path::Path) {
    for path in ["HEAD", "refs/tags", "packed-refs"] {
        if let Some(path) = git_output(repo, &["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={}", repo.join(path).display());
        }
    }
    if let Some(head_ref) = git_output(repo, &["symbolic-ref", "HEAD"]) {
        if let Some(path) = git_output(repo, &["rev-parse", "--git-path", &head_ref]) {
            println!("cargo:rerun-if-changed={}", repo.join(path).display());
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GITHUB_ACTIONS");

    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let release_metadata_path = repo_root.join(".openvmm-release.json");
    println!("cargo:rerun-if-changed={}", release_metadata_path.display());

    let release_metadata = read_release_metadata(&release_metadata_path);
    let git = version::collect_git_source(
        &repo_root,
        std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true"),
    )
    .unwrap_or_else(|error| panic!("failed to collect OpenVMM Git identity: {error}"));
    if git.is_some() {
        watch_git_inputs(&repo_root);
    }
    let version = version::resolve_version(git.as_ref(), release_metadata.as_ref())
        .unwrap_or_else(|error| panic!("failed to resolve OpenVMM version: {error}"));

    if git.is_none() && release_metadata.is_none() {
        println!(
            "cargo:warning=OpenVMM release metadata is unavailable. This build will report \
             0.0.0-dev and must not be treated as an official release build. Use a Git checkout \
             with the release tag available or the official source bundle attached to the GitHub \
             Release."
        );
    }

    println!(
        "cargo:rustc-env=OPENVMM_PRODUCT_VERSION={}",
        version.product_version
    );
    println!("cargo:rustc-env=OPENVMM_VERSION={}", version.version);
    println!("cargo:rustc-env=BUILD_GIT_SHA={}", version.revision);
}
