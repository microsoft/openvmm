// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides build metadata

#![expect(missing_docs)]

use inspect::Inspect;

#[derive(Debug, Inspect)]
pub struct DebugBuildInfo {
    #[inspect(safe)]
    author_name: &'static str,
    #[inspect(safe)]
    author_email: &'static str,
    #[inspect(safe)]
    build_timestamp: &'static str,
    #[inspect(safe)]
    build_machine: &'static str,
    #[inspect(safe)]
    built_with_uncommitted_changes: &'static str,
}

#[derive(Debug, Inspect)]
pub struct BaseBuildInfo {
    #[inspect(safe)]
    crate_name: &'static str,
    #[inspect(safe, rename = "scm_revision")]
    revision: &'static str,
    #[inspect(safe, rename = "scm_branch")]
    branch: &'static str,
    #[inspect(safe)]
    internal_scm_revision: &'static str,
    #[inspect(safe)]
    internal_scm_branch: &'static str,
    #[inspect(safe)]
    openhcl_version: &'static str,
}

#[derive(Debug, Inspect)]
#[inspect(untagged)]
pub enum BuildInfo {
    #[inspect(transparent)]
    WithoutDebugInfo(BaseBuildInfo),
    WithDebugInfo {
        #[inspect(flatten)]
        build: BaseBuildInfo,
        #[inspect(flatten)]
        debug_build_info: DebugBuildInfo,
    },
}

/// Helper to convert Option<&'static str> to &'static str in const context.
/// An empty string is returned if the Option is None.
const fn empty_if_none(env: Option<&'static str>) -> &'static str {
    if let Some(value) = env { value } else { "" }
}

impl BuildInfo {
    pub const fn new() -> Self {
        // TODO: Once Option::unwrap_or() is stable in the const context
        // can replace the empty_if_none helper with that.
        // Deliberately not storing `Option` to the build information
        // structure to be closer to PODs.
        let build = BaseBuildInfo {
            crate_name: env!("CARGO_PKG_NAME"),
            revision: empty_if_none(option_env!("VERGEN_GIT_SHA")),
            branch: empty_if_none(option_env!("VERGEN_GIT_BRANCH")),
            internal_scm_revision: empty_if_none(option_env!("INTERNAL_GIT_SHA")),
            internal_scm_branch: empty_if_none(option_env!("INTERNAL_GIT_BRANCH")),
            openhcl_version: empty_if_none(option_env!("OPENHCL_VERSION")),
        };
        if option_env!("INCLUDE_DEBUG_BUILD_INFO").is_some() {
            Self::WithDebugInfo {
                build,
                debug_build_info: DebugBuildInfo {
                    author_name: empty_if_none(option_env!("VERGEN_GIT_COMMIT_AUTHOR_NAME")),
                    author_email: empty_if_none(option_env!("VERGEN_GIT_COMMIT_AUTHOR_EMAIL")),
                    build_timestamp: empty_if_none(option_env!("SOURCE_DATE_EPOCH")),
                    build_machine: empty_if_none(option_env!("DEBUG_BUILD_INFO_MACHINE_NAME")),
                    built_with_uncommitted_changes: empty_if_none(option_env!(
                        "DEBUG_BUILD_INFO_UNCOMMITTED_CHANGES"
                    )),
                },
            }
        } else {
            Self::WithoutDebugInfo(build)
        }
    }

    pub fn crate_name(&self) -> &'static str {
        match self {
            BuildInfo::WithDebugInfo { build, .. } => build.crate_name,
            BuildInfo::WithoutDebugInfo(info) => info.crate_name,
        }
    }

    pub fn scm_revision(&self) -> &'static str {
        match self {
            BuildInfo::WithDebugInfo { build, .. } => build.revision,
            BuildInfo::WithoutDebugInfo(info) => info.revision,
        }
    }

    pub fn scm_branch(&self) -> &'static str {
        match self {
            BuildInfo::WithDebugInfo { build, .. } => build.branch,
            BuildInfo::WithoutDebugInfo(info) => info.branch,
        }
    }
}

// Placing into a separate section to make easier to discover
// the build information even without a debugger.
//
// The #[used] attribute is not used as the static is reachable
// via a public function.
//
// The #[external_name] attribute is used to give the static
// an unmangled name and again be easily discoverable even without
// a debugger. With a debugger, the non-mangled name is easier
// to use.

// UNSAFETY: link_section and export_name are unsafe.
#[expect(unsafe_code)]
// SAFETY: The build_info section is custom and carries no safety requirements.
#[unsafe(link_section = ".build_info")]
// SAFETY: The name "BUILD_INFO" is only declared here in OpenHCL and shouldn't
// collide with any other symbols. It is a special symbol intended for
// post-mortem debugging, and no runtime functionality should depend on it.
#[unsafe(export_name = "BUILD_INFO")]
static BUILD_INFO: BuildInfo = BuildInfo::new();

pub fn get() -> &'static BuildInfo {
    // Without `black_box`, BUILD_INFO is optimized away
    // in the release builds with `fat` LTO.
    std::hint::black_box(&BUILD_INFO)
}
