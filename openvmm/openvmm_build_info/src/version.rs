// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub struct GitSource {
    pub revision: String,
    pub dirty: bool,
}

pub struct VersionInfo {
    pub version: String,
    pub revision: String,
}

/// Resolves the reported build identity.
pub fn resolve_version(product_version: &str, git: Option<GitSource>) -> VersionInfo {
    let Some(git) = git else {
        return VersionInfo {
            version: product_version.to_owned(),
            revision: String::new(),
        };
    };

    let short_revision = &git.revision[..9.min(git.revision.len())];
    let dirty = if git.dirty { ".dirty" } else { "" };
    VersionInfo {
        version: format!("{product_version}+g{short_revision}{dirty}"),
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
            dirty: false,
        }
    }

    #[test]
    fn an_archive_reports_the_product_version() {
        let version = resolve_version("0.2.0", None);
        assert_eq!(version.version, "0.2.0");
        assert!(version.revision.is_empty());
    }

    #[test]
    fn an_ordinary_checkout_includes_the_revision() {
        let version = resolve_version("0.2.0", Some(git()));
        assert_eq!(version.version, "0.2.0+g012345678");
        assert_eq!(version.revision, REVISION);
    }

    #[test]
    fn a_dirty_checkout_is_marked() {
        let version = resolve_version(
            "0.2.0",
            Some(GitSource {
                dirty: true,
                ..git()
            }),
        );
        assert_eq!(version.version, "0.2.0+g012345678.dirty");
    }
}
