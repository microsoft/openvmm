// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM product version and source provenance.

#![expect(missing_docs)]

#[cfg(test)]
#[path = "../version.rs"]
mod version;

#[derive(Debug)]
pub struct BuildInfo {
    product_version: &'static str,
    version: &'static str,
    channel: &'static str,
    release_tag: &'static str,
    dirty: bool,
    revision: &'static str,
    branch: &'static str,
}

impl BuildInfo {
    pub const fn new() -> Self {
        Self {
            product_version: env!("OPENVMM_PRODUCT_VERSION"),
            version: env!("OPENVMM_VERSION"),
            channel: env!("OPENVMM_BUILD_CHANNEL"),
            release_tag: env!("OPENVMM_RELEASE_TAG"),
            dirty: matches!(env!("OPENVMM_SOURCE_DIRTY").as_bytes(), b"true"),
            revision: if let Some(revision) = option_env!("BUILD_GIT_SHA") {
                revision
            } else {
                ""
            },
            branch: if let Some(branch) = option_env!("BUILD_GIT_BRANCH") {
                branch
            } else {
                ""
            },
        }
    }

    pub const fn product_version(&self) -> &'static str {
        self.product_version
    }

    pub const fn version(&self) -> &'static str {
        self.version
    }

    pub const fn channel(&self) -> &'static str {
        self.channel
    }

    pub const fn release_tag(&self) -> &'static str {
        self.release_tag
    }

    pub const fn dirty(&self) -> bool {
        self.dirty
    }

    pub const fn scm_revision(&self) -> &'static str {
        self.revision
    }

    pub const fn scm_branch(&self) -> &'static str {
        self.branch
    }
}

// Keep the build information easy to discover without a debugger.
//
// The static remains reachable through `get`, so `#[used]` is not required.
//
// UNSAFETY: `link_section` and `export_name` are unsafe attributes.
#[expect(unsafe_code)]
// SAFETY: These are custom metadata sections with no safety requirements.
#[cfg_attr(target_os = "windows", unsafe(link_section = ".build_i"))]
#[cfg_attr(target_vendor = "apple", unsafe(link_section = "__DATA,__build_info"))]
#[cfg_attr(
    not(any(target_os = "windows", target_vendor = "apple")),
    unsafe(link_section = ".build_info")
)]
// SAFETY: This symbol is uniquely named for OpenVMM and has no runtime ABI.
#[unsafe(export_name = "OPENVMM_BUILD_INFO")]
static OPENVMM_BUILD_INFO: BuildInfo = BuildInfo::new();

pub fn get() -> &'static BuildInfo {
    // Prevent fat LTO from optimizing away the metadata static.
    std::hint::black_box(&OPENVMM_BUILD_INFO)
}
