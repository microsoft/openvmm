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

// Parse a `&str` segment as a u32. Panics if the segment is empty or not a valid u32.
const fn const_parse_u32_segment(s: &str) -> u32 {
    assert!(!s.is_empty(), "version segment must not be empty");
    match u32::from_str_radix(s, 10) {
        Ok(v) => v,
        Err(_) => panic!("version segment is not a valid u32"),
    }
}

// Const-compatible equivalent of `s.split_once('.')`.
const fn const_split_once_dot(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            let (left, dot_right) = s.split_at(i);
            let (_, right) = dot_right.split_at(1);
            return Some((left, right));
        }
        i += 1;
    }
    None
}

// Parse the `OPENHCL_VERSION` environment variable into four u32 components.
// Strictly enforces the format to be "major.minor.build.platform",
// where each is a valid u32.
// Empty string is permitted to not panic on builds which omit the
// `OPENHCL_VERSION` environment variable like `cargo test`.
const fn const_parse_version(s: &str) -> [u32; 4] {
    if s.is_empty() {
        return [0, 0, 0, 0];
    }
    let (major, rest) = match const_split_once_dot(s) {
        Some(pair) => pair,
        None => panic!("expected 4 dot-separated components in version string"),
    };
    let (minor, rest) = match const_split_once_dot(rest) {
        Some(pair) => pair,
        None => panic!("expected 4 dot-separated components in version string"),
    };
    let (build, platform) = match const_split_once_dot(rest) {
        Some(pair) => pair,
        None => panic!("expected 4 dot-separated components in version string"),
    };
    // The fourth segment must not contain additional dots.
    assert!(
        const_split_once_dot(platform).is_none(),
        "expected exactly 4 dot-separated components in version string"
    );
    let major = const_parse_u32_segment(major);
    let minor = const_parse_u32_segment(minor);
    let build = const_parse_u32_segment(build);
    let platform = const_parse_u32_segment(platform);
    [major, minor, build, platform]
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
    pub const fn new() -> Self {
        let [major, minor, build, platform] = const_parse_version(BuildInfo::new().openhcl_version);
        Self {
            product_name: "OpenHCL",
            major,
            minor,
            build,
            platform,
        }
    }

    pub const fn product_name(&self) -> &'static str {
        self.product_name
    }

    pub const fn major(&self) -> u32 {
        self.major
    }

    pub const fn minor(&self) -> u32 {
        self.minor
    }

    pub const fn build(&self) -> u32 {
        self.build
    }

    pub const fn platform(&self) -> u32 {
        self.platform
    }
}

pub static OPENHCL_VERSION: OpenHclVersion = OpenHclVersion::new();

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
        // Ensure running `cargo test` doesn't panic.
        assert_eq!(const_parse_version(""), [0, 0, 0, 0]);
    }

    #[test]
    fn full_version() {
        assert_eq!(const_parse_version("1.6.499.2"), [1, 6, 499, 2]);
    }

    #[test]
    fn zero_platform() {
        assert_eq!(const_parse_version("1.1.1.0"), [1, 1, 1, 0]);
    }

    #[test]
    fn zero_major_allowed() {
        assert_eq!(const_parse_version("0.1.1.0"), [0, 1, 1, 0]);
    }

    #[test]
    fn zero_minor_allowed() {
        assert_eq!(const_parse_version("1.0.1.0"), [1, 0, 1, 0]);
    }

    #[test]
    fn zero_build_allowed() {
        assert_eq!(const_parse_version("1.1.0.0"), [1, 1, 0, 0]);
    }

    #[test]
    #[should_panic(expected = "expected 4 dot-separated components")]
    fn partial_version_panics() {
        const_parse_version("1.2");
    }

    #[test]
    #[should_panic(expected = "version segment is not a valid u32")]
    fn non_digit_segment_panics() {
        const_parse_version("1.2.3A.4");
    }

    #[test]
    #[should_panic(expected = "expected exactly 4 dot-separated components")]
    fn extra_segments_panics() {
        const_parse_version("1.2.3.4.5");
    }

    #[test]
    #[should_panic(expected = "expected 4 dot-separated components")]
    fn single_component_panics() {
        const_parse_version("42");
    }

    #[test]
    #[should_panic(expected = "version segment is not a valid u32")]
    fn overflow_panics() {
        const_parse_version("9999999999.0.0.0");
    }

    #[test]
    #[should_panic(expected = "version segment must not be empty")]
    fn empty_segment_panics() {
        const_parse_version("1..3.4");
    }
}
