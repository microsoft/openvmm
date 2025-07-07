// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Provides build metadata

#![expect(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;
use inspect::Inspect;

#[derive(Debug, Inspect)]
pub struct BuildInfo {
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
    #[inspect(safe)]
    build_profile: &'static str,
    #[inspect(safe)]
    target_arch: &'static str,
    #[inspect(safe)]
    arbitrary_target: &'static str,
    #[inspect(safe)]
    arbitrary_features: &'static str,
    #[inspect(safe)]
    arbitrary_timestamp: &'static str,
    #[inspect(safe)]
    arbitrary_rust_version: &'static str,
    #[inspect(safe)]
    arbitrary_custom_1: &'static str,
    #[inspect(safe)]
    arbitrary_custom_2: &'static str,
    #[inspect(safe)]
    arbitrary_custom_3: &'static str,
    #[inspect(safe)]
    arbitrary_custom_4: &'static str,
    #[inspect(safe)]
    arbitrary_custom_5: &'static str,
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
            internal_scm_revision: if let Some(r) = option_env!("INTERNAL_GIT_SHA") {
                r
            } else {
                ""
            },
            internal_scm_branch: if let Some(r) = option_env!("INTERNAL_GIT_BRANCH") {
                r
            } else {
                ""
            },
            openhcl_version: if let Some(r) = option_env!("OPENHCL_VERSION") {
                r
            } else {
                ""
            },
            build_profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            },
            target_arch: if let Some(arch) = option_env!("OPENVMM_BUILD_TARGET_ARCH") {
                arch
            } else {
                // Default to unknown if not set by build script
                "unknown"
            },
            arbitrary_target: if let Some(r) = option_env!("OPENVMM_BUILD_TARGET") {
                r
            } else {
                ""
            },
            arbitrary_features: if let Some(r) = option_env!("OPENVMM_BUILD_FEATURES") {
                r
            } else {
                ""
            },
            arbitrary_timestamp: if let Some(r) = option_env!("OPENVMM_BUILD_TIMESTAMP") {
                r
            } else {
                ""
            },
            arbitrary_rust_version: if let Some(r) = option_env!("OPENVMM_BUILD_RUST_VERSION") {
                r
            } else {
                ""
            },
            arbitrary_custom_1: if let Some(r) = option_env!("OPENVMM_BUILD_CUSTOM_1") {
                r
            } else {
                ""
            },
            arbitrary_custom_2: if let Some(r) = option_env!("OPENVMM_BUILD_CUSTOM_2") {
                r
            } else {
                ""
            },
            arbitrary_custom_3: if let Some(r) = option_env!("OPENVMM_BUILD_CUSTOM_3") {
                r
            } else {
                ""
            },
            arbitrary_custom_4: if let Some(r) = option_env!("OPENVMM_BUILD_CUSTOM_4") {
                r
            } else {
                ""
            },
            arbitrary_custom_5: if let Some(r) = option_env!("OPENVMM_BUILD_CUSTOM_5") {
                r
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

    /// Get the build profile (debug or release)
    pub fn build_profile(&self) -> &'static str {
        self.build_profile
    }

    /// Get the target architecture
    pub fn target_arch(&self) -> &'static str {
        self.target_arch
    }

    /// Get arbitrary build data by key
    pub fn get_arbitrary_data(&self, key: &str) -> Option<&'static str> {
        match key {
            "build_profile" => Some(self.build_profile),
            "target_arch" => Some(self.target_arch),
            "target" => if self.arbitrary_target.is_empty() { None } else { Some(self.arbitrary_target) },
            "features" => if self.arbitrary_features.is_empty() { None } else { Some(self.arbitrary_features) },
            "timestamp" => if self.arbitrary_timestamp.is_empty() { None } else { Some(self.arbitrary_timestamp) },
            "rust_version" => if self.arbitrary_rust_version.is_empty() { None } else { Some(self.arbitrary_rust_version) },
            "custom_1" => if self.arbitrary_custom_1.is_empty() { None } else { Some(self.arbitrary_custom_1) },
            "custom_2" => if self.arbitrary_custom_2.is_empty() { None } else { Some(self.arbitrary_custom_2) },
            "custom_3" => if self.arbitrary_custom_3.is_empty() { None } else { Some(self.arbitrary_custom_3) },
            "custom_4" => if self.arbitrary_custom_4.is_empty() { None } else { Some(self.arbitrary_custom_4) },
            "custom_5" => if self.arbitrary_custom_5.is_empty() { None } else { Some(self.arbitrary_custom_5) },
            _ => None,
        }
    }

    /// Get all arbitrary build data as key-value pairs (non-empty values only)
    pub fn arbitrary_data(&self) -> Vec<(&'static str, &'static str)> {
        let mut data = Vec::new();
        
        data.push(("build_profile", self.build_profile));
        data.push(("target_arch", self.target_arch));
        
        if !self.arbitrary_target.is_empty() {
            data.push(("target", self.arbitrary_target));
        }
        if !self.arbitrary_features.is_empty() {
            data.push(("features", self.arbitrary_features));
        }
        if !self.arbitrary_timestamp.is_empty() {
            data.push(("timestamp", self.arbitrary_timestamp));
        }
        if !self.arbitrary_rust_version.is_empty() {
            data.push(("rust_version", self.arbitrary_rust_version));
        }
        if !self.arbitrary_custom_1.is_empty() {
            data.push(("custom_1", self.arbitrary_custom_1));
        }
        if !self.arbitrary_custom_2.is_empty() {
            data.push(("custom_2", self.arbitrary_custom_2));
        }
        if !self.arbitrary_custom_3.is_empty() {
            data.push(("custom_3", self.arbitrary_custom_3));
        }
        if !self.arbitrary_custom_4.is_empty() {
            data.push(("custom_4", self.arbitrary_custom_4));
        }
        if !self.arbitrary_custom_5.is_empty() {
            data.push(("custom_5", self.arbitrary_custom_5));
        }
        
        data
    }

    /// Check if this is a debug build
    pub fn is_debug_build(&self) -> bool {
        self.build_profile == "debug"
    }

    /// Check if this is a release build
    pub fn is_release_build(&self) -> bool {
        self.build_profile == "release"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_info_basic() {
        let build_info = BuildInfo::new();
        
        // Test basic fields
        assert_eq!(build_info.crate_name(), "build_info");
        assert!(!build_info.build_profile().is_empty());
        assert!(!build_info.target_arch().is_empty());
        
        // Test build profile detection
        #[cfg(debug_assertions)]
        assert!(build_info.is_debug_build());
        #[cfg(not(debug_assertions))]
        assert!(build_info.is_release_build());
    }

    #[test]
    fn test_arbitrary_data() {
        let build_info = BuildInfo::new();
        
        // Test build profile is always available
        assert_eq!(build_info.get_arbitrary_data("build_profile"), Some(build_info.build_profile()));
        
        // Test target arch is always available
        assert_eq!(build_info.get_arbitrary_data("target_arch"), Some(build_info.target_arch()));
        
        // Test non-existent key returns None
        assert_eq!(build_info.get_arbitrary_data("non_existent"), None);
        
        // Test arbitrary data collection
        let data = build_info.arbitrary_data();
        assert!(!data.is_empty());
        
        // Verify build_profile and target_arch are in the data
        assert!(data.iter().any(|(k, _)| *k == "build_profile"));
        assert!(data.iter().any(|(k, _)| *k == "target_arch"));
    }

    #[test]
    fn test_get_function() {
        let build_info = crate::get();
        
        // Test that get() returns the same data as new()
        assert_eq!(build_info.crate_name(), "build_info");
        assert!(!build_info.build_profile().is_empty());
        assert!(!build_info.target_arch().is_empty());
    }

    #[test]
    fn test_debug_release_detection() {
        let build_info = BuildInfo::new();
        
        // Only one of these should be true
        assert!(build_info.is_debug_build() ^ build_info.is_release_build());
        
        // At least one should be true
        assert!(build_info.is_debug_build() || build_info.is_release_build());
    }
}
