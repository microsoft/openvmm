// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]

use serde::Deserialize;

mod version;

const RELEASE_METADATA_SCHEMA: u32 = 1;
const SOURCE_METADATA_SCHEMA: u32 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SerializedReleaseMetadata {
    schema_version: u32,
    version: String,
    tag: String,
    revision: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SerializedSourceMetadata {
    schema_version: u32,
    revision: String,
    branch: String,
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

fn read_source_metadata(path: &std::path::Path) -> Option<version::SourceMetadata> {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("failed to read {}: {error}", path.display()),
    };
    let metadata: SerializedSourceMetadata = serde_json::from_slice(&contents)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()));
    assert_eq!(
        metadata.schema_version, SOURCE_METADATA_SCHEMA,
        "unsupported OpenVMM source metadata schema"
    );
    let metadata = version::SourceMetadata {
        revision: metadata.revision,
        branch: metadata.branch,
    };
    version::validate_source_metadata(&metadata)
        .unwrap_or_else(|error| panic!("invalid {}: {error}", path.display()));
    Some(metadata)
}

fn git_source(
    git_info: &build_rs_git_info::GitInfo,
    ignore_ci_config_rewrite: bool,
) -> version::GitSource<'_> {
    version::GitSource {
        sha: git_info.sha(),
        branch: git_info.branch(),
        tags: git_info.tags(),
        dirty: git_info.dirty() && !ignore_ci_config_rewrite,
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=GITHUB_ACTIONS");
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let release_metadata_path = repo_root.join(".openvmm-release.json");
    let source_metadata_path = repo_root.join(".openvmm-source.json");
    println!("cargo:rerun-if-changed={}", release_metadata_path.display());
    println!("cargo:rerun-if-changed={}", source_metadata_path.display());

    let git_info = match build_rs_git_info::collect_git_info_at(&repo_root) {
        Ok(git_info) => Some(git_info),
        Err(error) => {
            println!("cargo:warning=failed to collect OpenVMM git build information: {error:#}");
            None
        }
    };
    if git_info
        .as_ref()
        .is_some_and(|git_info| git_info.shallow() && git_info.branch() == "HEAD")
        && !git_info.as_ref().is_some_and(|git_info| {
            git_info
                .tags()
                .iter()
                .any(|tag| tag.starts_with("openvmm-v"))
        })
    {
        println!(
            "cargo:warning=OpenVMM is being built from a shallow detached checkout without an \
             exact release tag. The build will report a development version; fetch tags if this \
             commit is expected to be a release."
        );
    }
    let release_metadata = read_release_metadata(&release_metadata_path);
    let source_metadata = read_source_metadata(&source_metadata_path);
    let generated_metadata = release_metadata
        .as_ref()
        .map(version::GeneratedMetadata::Release)
        .or_else(|| {
            source_metadata
                .as_ref()
                .map(version::GeneratedMetadata::Source)
        });
    let ignore_ci_config_rewrite = version::ci_config_rewrite_is_only_change(
        &repo_root,
        std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true"),
    );
    let version = version::resolve_version(
        git_info
            .as_ref()
            .map(|git_info| git_source(git_info, ignore_ci_config_rewrite)),
        generated_metadata,
    )
    .unwrap_or_else(|error| panic!("failed to resolve OpenVMM version: {error}"));
    if git_info.is_none() && release_metadata.is_none() && source_metadata.is_none() {
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
    println!("cargo:rustc-env=OPENVMM_BUILD_CHANNEL={}", version.channel);
    println!("cargo:rustc-env=OPENVMM_VERSION={}", version.version);
    println!(
        "cargo:rustc-env=OPENVMM_RELEASE_TAG={}",
        version.release_tag
    );
    println!("cargo:rustc-env=OPENVMM_SOURCE_DIRTY={}", version.dirty);
    println!("cargo:rustc-env=BUILD_GIT_SHA={}", version.revision);
    println!("cargo:rustc-env=BUILD_GIT_BRANCH={}", version.branch);
}
