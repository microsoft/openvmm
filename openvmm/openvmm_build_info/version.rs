// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildKind {
    Release,
    Development,
}

impl BuildKind {
    pub const fn description(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::Development => "development (not an official release)",
        }
    }
}

pub struct GitSource {
    pub revision: String,
}

pub struct VersionInfo {
    pub version: String,
    pub kind: BuildKind,
    pub revision: String,
}

pub fn resolve_version(product_version: &str, git: Option<GitSource>) -> VersionInfo {
    let Some(git) = git else {
        return VersionInfo {
            version: product_version.to_owned(),
            kind: BuildKind::Release,
            revision: String::new(),
        };
    };

    let short_revision = &git.revision[..9.min(git.revision.len())];
    VersionInfo {
        version: format!("{product_version}+g{short_revision}"),
        kind: BuildKind::Development,
        revision: git.revision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const REVISION: &str = "0123456789abcdef0123456789abcdef01234567";

    fn git() -> GitSource {
        GitSource {
            revision: REVISION.into(),
        }
    }

    #[test]
    fn an_archive_reports_the_product_version() {
        let version = resolve_version("0.2.0", None);
        assert_eq!(version.version, "0.2.0");
        assert_eq!(version.kind, BuildKind::Release);
        assert!(version.revision.is_empty());
    }

    #[test]
    fn an_ordinary_checkout_includes_the_revision() {
        let version = resolve_version("0.2.0", Some(git()));
        assert_eq!(version.version, "0.2.0+g012345678");
        assert_eq!(version.kind, BuildKind::Development);
        assert_eq!(version.revision, REVISION);
    }

    #[test]
    fn build_kind_describes_development_builds() {
        assert_eq!(BuildKind::Release.description(), "release");
        assert!(
            BuildKind::Development
                .description()
                .contains("not an official release")
        );
    }
}
