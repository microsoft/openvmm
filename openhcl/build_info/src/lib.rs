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
    #[inspect(safe)]
    internal_scm_revision: &'static str,
    #[inspect(safe)]
    internal_scm_branch: &'static str,
    #[inspect(safe)]
    openhcl_version: &'static str,
}

impl BuildInfo {
    pub const fn new() -> Self {
        // TODO: Once Option::unwrap_or() is stable in the const context
        // can replace the if statements with it.
        // Deliberately not storing `Option` to the build information
        // structure to be closer to PODs.
        Self {
            crate_name: env!("CARGO_PKG_NAME"),
            revision: if let Some(r) = option_env!("BUILD_GIT_SHA") {
                r
            } else {
                ""
            },
            branch: if let Some(b) = option_env!("BUILD_GIT_BRANCH") {
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

    pub fn openhcl_version(&self) -> &'static str {
        self.openhcl_version
    }
}

// Parse `bytes[start..end]` as a u32. Returns 0 if the segment is empty
// or contains any non-digit character.
const fn const_parse_u32_segment(bytes: &[u8], start: usize, end: usize) -> u32 {
    if start >= end {
        return 0;
    }
    let mut acc = 0u32;
    let mut i = start;
    while i < end {
        let b = bytes[i];
        if b < b'0' || b > b'9' {
            return 0;
        }
        acc = acc.saturating_mul(10).saturating_add((b - b'0') as u32);
        i += 1;
    }
    acc
}

// Parse the `OPENHCL_VERSION` env var (format "major.minor.build.platform")
// into four u32 components. Missing or invalid components default to 0.
const fn const_parse_version(s: &str) -> (u32, u32, u32, u32) {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut components = [0u32; 4];
    let mut comp = 0;
    let mut seg_start = 0;
    let mut i = 0;
    while i <= len && comp < 4 {
        if i == len || bytes[i] == b'.' {
            components[comp] = const_parse_u32_segment(bytes, seg_start, i);
            comp += 1;
            seg_start = i + 1;
        }
        i += 1;
    }
    (components[0], components[1], components[2], components[3])
}

/// Parsed components of the OPENHCL_VERSION env var (major.minor.build.platform).
/// All parsing happens at compile time — components are stored as u32.
#[derive(Debug)]
pub struct OpenHclVersion {
    product_name: &'static str,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
}

impl OpenHclVersion {
    const VERSION: (u32, u32, u32, u32) = const_parse_version(BuildInfo::new().openhcl_version);

    pub const fn new() -> Self {
        Self {
            product_name: "OpenHCL",
            major: Self::VERSION.0,
            minor: Self::VERSION.1,
            build: Self::VERSION.2,
            platform: Self::VERSION.3,
        }
    }

    pub fn product_name(&self) -> &'static str {
        self.product_name
    }

    pub fn major(&self) -> u32 {
        self.major
    }

    pub fn minor(&self) -> u32 {
        self.minor
    }

    pub fn build(&self) -> u32 {
        self.build
    }

    pub fn platform(&self) -> u32 {
        self.platform
    }
}

static OPENHCL_VERSION: OpenHclVersion = OpenHclVersion::new();

pub fn openhcl_version() -> &'static OpenHclVersion {
    &OPENHCL_VERSION
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
    use super::const_parse_version;

    #[test]
    fn empty_string() {
        assert_eq!(const_parse_version(""), (0, 0, 0, 0));
    }

    #[test]
    fn full_version() {
        assert_eq!(const_parse_version("1.2.3.4"), (1, 2, 3, 4));
    }

    #[test]
    fn partial_version() {
        assert_eq!(const_parse_version("1.2"), (1, 2, 0, 0));
    }

    #[test]
    fn non_digit_segment() {
        assert_eq!(const_parse_version("1.abc.3.4"), (1, 0, 3, 4));
    }

    #[test]
    fn extra_segments_ignored() {
        assert_eq!(const_parse_version("1.2.3.4.5"), (1, 2, 3, 4));
    }

    #[test]
    fn single_component() {
        assert_eq!(const_parse_version("42"), (42, 0, 0, 0));
    }

    #[test]
    fn overflow_saturates() {
        assert_eq!(const_parse_version("9999999999.0.0.0"), (u32::MAX, 0, 0, 0));
    }
}
