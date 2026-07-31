// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! OpenVMM product version and source revision.
//!
//! The version is inherited from `[workspace.package]` in the root
//! `Cargo.toml`, which is the source of truth for what a release is cut at. It
//! therefore travels inside the source archive, and a packager building an
//! extracted tree with no Git history still reports the right version. Git is
//! consulted only to distinguish the exact release tag from any other checkout
//! and append the revision to the latter.

#![expect(missing_docs)]

/// What kind of build this is.
///
/// Deliberately not a boolean "official". OpenVMM ships as source that someone
/// else builds, so a packager's binary is legitimately not ours and yet is a
/// legitimate build of an official version. The proof that a *source release*
/// is official is the build provenance attestation on the archive, not anything
/// a binary can claim about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    /// Built from the released source: either the published archive or an exact
    /// checkout of its tag.
    Release,
    /// Built from any other checkout.
    Development,
    /// Built with `OPENVMM_PKGVERSION`, so the reported version is whatever
    /// the builder chose.
    Custom,
}

impl BuildKind {
    const fn resolve() -> Self {
        // `match` on bytes because `str` comparison is not const.
        match env!("OPENVMM_BUILD_KIND").as_bytes() {
            b"release" => BuildKind::Release,
            b"development" => BuildKind::Development,
            _ => BuildKind::Custom,
        }
    }

    /// Whether this build came from the released source.
    ///
    /// This says nothing about *who* built it.
    pub const fn is_release(&self) -> bool {
        matches!(self, BuildKind::Release)
    }
}

/// Version and source identity of this build.
#[derive(Debug)]
pub struct BuildInfo {
    version: &'static str,
    long_version: &'static str,
    product_version: &'static str,
    revision: &'static str,
    target: &'static str,
    kind: BuildKind,
}

impl BuildInfo {
    pub const fn new() -> Self {
        Self {
            version: env!("OPENVMM_VERSION"),
            long_version: include_str!(concat!(env!("OUT_DIR"), "/long_version.txt")),
            product_version: env!("OPENVMM_PRODUCT_VERSION"),
            revision: env!("OPENVMM_REVISION"),
            target: env!("OPENVMM_TARGET"),
            kind: BuildKind::resolve(),
        }
    }

    /// The version to show a human, as reported by `openvmm -V`.
    ///
    /// This carries whatever enrichment was available at build time: a `+g`
    /// revision suffix for a checkout other than the exact release tag, or a
    /// packager's own string if they set `OPENVMM_PKGVERSION`.
    pub const fn version(&self) -> &'static str {
        self.version
    }

    /// The multi-line form reported by `openvmm --version`, in the style of
    /// `rustc -vV`.
    pub const fn long_version(&self) -> &'static str {
        self.long_version
    }

    /// The plain upstream version, with nothing appended and no override
    /// applied.
    ///
    /// Use this rather than [`BuildInfo::version`] anywhere the value is
    /// persisted or compared, since it is the only part that is guaranteed to
    /// be an OpenVMM version and not a distribution's build string.
    pub const fn product_version(&self) -> &'static str {
        self.product_version
    }

    /// The commit this was built from, or empty if it was not built from a
    /// checkout (a released source archive, for instance).
    pub const fn scm_revision(&self) -> &'static str {
        self.revision
    }

    /// The target triple this was built for.
    pub const fn target(&self) -> &'static str {
        self.target
    }

    /// Whether this is a release, development, or custom build.
    pub const fn kind(&self) -> BuildKind {
        self.kind
    }
}

impl Default for BuildInfo {
    fn default() -> Self {
        Self::new()
    }
}

// Keep the build information easy to discover without a debugger, and without
// running the binary at all.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The version is only meaningful if this crate actually carries one.
    /// Cargo silently defaults a crate with no `version` to `0.0.0`, and the
    /// house-rules lint that normally *strips* `version` merely permits one
    /// here -- it cannot require it. So if `version.workspace = true` were
    /// dropped from Cargo.toml, every build would keep succeeding and every
    /// binary would quietly report `0.0.0`. This is the only thing that would
    /// notice.
    #[test]
    fn product_version_is_not_the_cargo_default() {
        assert_ne!(get().product_version(), "0.0.0");
        assert!(!get().version().is_empty());
    }

    /// A revision is optional, but a partial one means the build script
    /// mangled it.
    #[test]
    fn revision_is_a_full_object_id_or_absent() {
        let revision = get().scm_revision();
        assert!(
            revision.is_empty()
                || (matches!(revision.len(), 40 | 64)
                    && revision.bytes().all(|b| b.is_ascii_hexdigit())),
            "unexpected revision {revision:?}"
        );
    }

    /// Only the released source reports the plain product version.
    #[test]
    fn only_a_release_build_reports_the_plain_version() {
        let info = get();
        match info.kind() {
            BuildKind::Development => {
                assert!(!info.kind().is_release());
                assert_eq!(
                    info.version(),
                    format!("{}+g{}", info.product_version(), &info.scm_revision()[..9])
                );
            }
            BuildKind::Release => {
                assert!(info.kind().is_release());
                assert_eq!(info.version(), info.product_version());
            }
            BuildKind::Custom => {}
        }
    }

    /// The long form is what a bug report gets pasted from, so it has to carry
    /// the parts that identify the build.
    #[test]
    fn long_version_carries_the_identity() {
        let info = get();
        let long = info.long_version();
        assert!(long.starts_with(info.version()), "{long:?}");
        assert!(long.contains(info.product_version()), "{long:?}");
        assert!(long.contains(info.target()), "{long:?}");
        if !info.scm_revision().is_empty() {
            assert!(long.contains(info.scm_revision()), "{long:?}");
        }
    }
}
