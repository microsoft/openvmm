// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides build metadata

#![expect(missing_docs)]
use inspect::Inspect;

#[derive(Debug, Inspect)]
pub struct BuildInfo {
    #[inspect(safe)]
    crate_name: &'static str,
    #[inspect(safe, rename = "scm_revision")]
    revision: &'static str,
    #[inspect(safe, rename = "scm_branch")]
    branch: &'static str,
}

impl BuildInfo {
    pub const fn new() -> Self {
        // TODO: Once Option::unwrap_or() is stable in the const context
        // can replace the if statements with it.
        // Deliberately not storing `Option` to the build information
        // structure to be closer to PODs.
        Self {
            crate_name: env!("CARGO_PKG_NAME"),
            revision: if let Some(r) = option_env!("VERGEN_GIT_SHA") {
                r
            } else {
                ""
            },
            branch: if let Some(b) = option_env!("VERGEN_GIT_BRANCH") {
                b
            } else {
                ""
            },
        }
    }

    pub fn crate_name(&self) -> &'static str {
        self.crate_name
    }

    pub fn scm_revision(&self) -> &'static str {
        self.revision
    }

    pub fn scm_branch(&self) -> &'static str {
        self.branch
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
