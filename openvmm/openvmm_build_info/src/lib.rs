// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM product version and source revision.

#![expect(missing_docs)]

#[cfg(test)]
#[path = "../version.rs"]
mod version;

#[derive(Debug)]
pub struct BuildInfo {
    product_version: &'static str,
    version: &'static str,
    revision: &'static str,
}

impl BuildInfo {
    pub const fn new() -> Self {
        Self {
            product_version: env!("OPENVMM_PRODUCT_VERSION"),
            version: env!("OPENVMM_VERSION"),
            revision: env!("BUILD_GIT_SHA"),
        }
    }

    pub const fn product_version(&self) -> &'static str {
        self.product_version
    }

    pub const fn version(&self) -> &'static str {
        self.version
    }

    pub const fn scm_revision(&self) -> &'static str {
        self.revision
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
