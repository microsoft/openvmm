// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

const RELEASE_TAG_PREFIX: &str = "openvmm-v";

pub struct ReleaseMetadata {
    pub version: String,
    pub tag: String,
    pub revision: String,
}

pub struct SourceMetadata {
    pub revision: String,
    pub branch: String,
}

pub enum GeneratedMetadata<'a> {
    Release(&'a ReleaseMetadata),
    Source(&'a SourceMetadata),
}

pub struct GitSource<'a> {
    pub sha: &'a str,
    pub branch: &'a str,
    pub tags: &'a [String],
    pub dirty: bool,
}

pub struct VersionInfo {
    pub product_version: String,
    pub version: String,
    pub channel: &'static str,
    pub release_tag: String,
    pub dirty: bool,
    pub revision: String,
    pub branch: String,
}

pub fn parse_version(version: &str) -> Result<[u16; 3], String> {
    let components = version.split('.').collect::<Vec<_>>();
    let [major, minor, patch] = components.as_slice() else {
        return Err(format!(
            "OpenVMM release version must contain exactly three components, got {version:?}"
        ));
    };
    let parse = |name: &str, component: &str| {
        if component.len() > 1 && component.starts_with('0') {
            return Err(format!(
                "OpenVMM release {name} component is not canonical: {component:?}"
            ));
        }
        component.parse::<u16>().map_err(|_| {
            format!("OpenVMM release {name} component must be an unsigned 16-bit integer")
        })
    };
    Ok([
        parse("major", major)?,
        parse("minor", minor)?,
        parse("patch", patch)?,
    ])
}

pub fn parse_release_tag(tag: &str) -> Result<&str, String> {
    let version = tag.strip_prefix(RELEASE_TAG_PREFIX).ok_or_else(|| {
        format!("OpenVMM release tag must start with {RELEASE_TAG_PREFIX:?}, got {tag:?}")
    })?;
    parse_version(version)?;
    Ok(version)
}

fn validate_revision(revision: &str) -> Result<(), String> {
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("OpenVMM release revision must be a full hexadecimal Git object ID".into());
    }
    Ok(())
}

pub fn validate_release_metadata(metadata: &ReleaseMetadata) -> Result<(), String> {
    let version = parse_release_tag(&metadata.tag)?;
    if metadata.version != version {
        return Err("OpenVMM release metadata tag and version do not match".into());
    }
    validate_revision(&metadata.revision)
}

pub fn validate_source_metadata(metadata: &SourceMetadata) -> Result<(), String> {
    validate_revision(&metadata.revision)
}

fn exact_release_tag(tags: &[String]) -> Result<Option<&str>, String> {
    let tags = tags
        .iter()
        .filter(|tag| tag.starts_with(RELEASE_TAG_PREFIX))
        .collect::<Vec<_>>();
    match tags.as_slice() {
        [] => Ok(None),
        [tag] => {
            parse_release_tag(tag)?;
            Ok(Some(tag))
        }
        _ => Err(format!(
            "multiple OpenVMM release tags point at HEAD: {tags:?}"
        )),
    }
}

pub fn resolve_version(
    git: Option<GitSource<'_>>,
    metadata: Option<GeneratedMetadata<'_>>,
) -> Result<VersionInfo, String> {
    if let Some(git) = &git {
        if let Some(tag) = exact_release_tag(git.tags)? {
            let product_version = parse_release_tag(tag)?.to_owned();
            let version = if git.dirty {
                format!("{product_version}+dirty")
            } else {
                product_version.clone()
            };
            return Ok(VersionInfo {
                product_version,
                version,
                channel: "release",
                release_tag: tag.to_owned(),
                dirty: git.dirty,
                revision: git.sha.to_owned(),
                branch: git.branch.to_owned(),
            });
        }
    }

    match metadata {
        Some(GeneratedMetadata::Release(metadata)) => {
            validate_release_metadata(metadata)?;
            return Ok(VersionInfo {
                product_version: metadata.version.clone(),
                version: metadata.version.clone(),
                channel: "release",
                release_tag: metadata.tag.clone(),
                dirty: false,
                revision: metadata.revision.clone(),
                branch: String::new(),
            });
        }
        Some(GeneratedMetadata::Source(metadata)) => {
            validate_source_metadata(metadata)?;
            let revision = metadata.revision.get(..9).ok_or_else(|| {
                format!(
                    "OpenVMM source revision is too short: {:?}",
                    metadata.revision
                )
            })?;
            return Ok(VersionInfo {
                product_version: "0.0.0".into(),
                version: format!("0.0.0-dev+g{revision}"),
                channel: "dev",
                release_tag: String::new(),
                dirty: false,
                revision: metadata.revision.clone(),
                branch: metadata.branch.clone(),
            });
        }
        None => {}
    }

    if let Some(git) = git {
        let revision = git
            .sha
            .get(..9)
            .ok_or_else(|| format!("OpenVMM Git revision is too short: {:?}", git.sha))?;
        let dirty = if git.dirty { ".dirty" } else { "" };
        return Ok(VersionInfo {
            product_version: "0.0.0".into(),
            version: format!("0.0.0-dev+g{revision}{dirty}"),
            channel: "dev",
            release_tag: String::new(),
            dirty: git.dirty,
            revision: git.sha.to_owned(),
            branch: git.branch.to_owned(),
        });
    }

    Ok(VersionInfo {
        product_version: "0.0.0".into(),
        version: "0.0.0-dev".into(),
        channel: "dev",
        release_tag: String::new(),
        dirty: false,
        revision: String::new(),
        branch: String::new(),
    })
}

pub fn ci_config_rewrite_is_only_change(repo_root: &std::path::Path, github_actions: bool) -> bool {
    if !github_actions {
        return false;
    }

    let config_path = repo_root.join(".cargo/config.toml");
    let Ok(current) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(committed) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", "HEAD:.cargo/config.toml"])
        .output()
    else {
        return false;
    };
    if !committed.status.success()
        || current != String::from_utf8_lossy(&committed.stdout).replace("### ENABLE_IN_CI", "")
    {
        return false;
    }

    let Ok(other_changes) = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--",
            ".",
            ":(exclude).cargo/config.toml",
        ])
        .output()
    else {
        return false;
    };
    other_changes.status.success() && other_changes.stdout.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn git(tags: &[&str], dirty: bool) -> GitSource<'static> {
        GitSource {
            sha: SHA,
            branch: "main",
            tags: Box::leak(
                tags.iter()
                    .map(|tag| (*tag).to_owned())
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            ),
            dirty,
        }
    }

    fn run(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn temporary_repo() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let repo = std::env::temp_dir().join(format!(
            "openvmm-version-info-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&repo).unwrap();
        run(&repo, &["init", "--quiet"]);
        run(&repo, &["config", "core.autocrlf", "false"]);
        run(&repo, &["config", "core.safecrlf", "false"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "Build Test"]);
        std::fs::create_dir(repo.join(".cargo")).unwrap();
        std::fs::write(
            repo.join(".cargo/config.toml"),
            "[build]\n### ENABLE_IN_CI rustflags = [\"-Dwarnings\"]\n",
        )
        .unwrap();
        run(&repo, &["add", ".cargo/config.toml"]);
        run(&repo, &["commit", "--quiet", "-m", "initial"]);
        repo
    }

    #[test]
    fn exact_tag_is_release_version() {
        let version = resolve_version(Some(git(&["openvmm-v0.12.3"], false)), None).unwrap();
        assert_eq!(version.product_version, "0.12.3");
        assert_eq!(version.version, "0.12.3");
        assert_eq!(version.channel, "release");
        assert_eq!(version.release_tag, "openvmm-v0.12.3");
        assert_eq!(version.revision, SHA);
        assert_eq!(version.branch, "main");
        assert!(!version.dirty);
    }

    #[test]
    fn dirty_exact_tag_is_marked_dirty() {
        let version = resolve_version(Some(git(&["openvmm-v0.12.3"], true)), None).unwrap();
        assert_eq!(version.product_version, "0.12.3");
        assert_eq!(version.version, "0.12.3+dirty");
        assert!(version.dirty);
    }

    #[test]
    fn untagged_git_is_development_version() {
        let version = resolve_version(Some(git(&[], true)), None).unwrap();
        assert_eq!(version.product_version, "0.0.0");
        assert_eq!(version.version, "0.0.0-dev+g012345678.dirty");
        assert_eq!(version.channel, "dev");
        assert_eq!(version.revision, SHA);
    }

    #[test]
    fn generated_metadata_restores_release_identity() {
        let metadata = ReleaseMetadata {
            version: "0.12.3".into(),
            tag: "openvmm-v0.12.3".into(),
            revision: SHA.into(),
        };
        let version = resolve_version(None, Some(GeneratedMetadata::Release(&metadata))).unwrap();
        assert_eq!(version.version, "0.12.3");
        assert_eq!(version.release_tag, "openvmm-v0.12.3");
        assert_eq!(version.revision, SHA);
    }

    #[test]
    fn generated_metadata_precedes_untagged_parent_git() {
        let metadata = ReleaseMetadata {
            version: "0.12.3".into(),
            tag: "openvmm-v0.12.3".into(),
            revision: SHA.into(),
        };
        let version = resolve_version(
            Some(git(&[], false)),
            Some(GeneratedMetadata::Release(&metadata)),
        )
        .unwrap();
        assert_eq!(version.version, "0.12.3");
        assert_eq!(version.revision, SHA);
    }

    #[test]
    fn exact_tag_precedes_generated_metadata() {
        let metadata = ReleaseMetadata {
            version: "0.12.2".into(),
            tag: "openvmm-v0.12.2".into(),
            revision: "abcdef0123456789abcdef0123456789abcdef01".into(),
        };
        let version = resolve_version(
            Some(git(&["openvmm-v0.12.3"], false)),
            Some(GeneratedMetadata::Release(&metadata)),
        )
        .unwrap();
        assert_eq!(version.version, "0.12.3");
        assert_eq!(version.revision, SHA);
    }

    #[test]
    fn no_source_metadata_has_generic_development_version() {
        let version = resolve_version(None, None).unwrap();
        assert_eq!(version.version, "0.0.0-dev");
        assert!(version.revision.is_empty());
    }

    #[test]
    fn generated_source_metadata_restores_development_revision() {
        let metadata = SourceMetadata {
            revision: SHA.into(),
            branch: "main".into(),
        };
        let version = resolve_version(None, Some(GeneratedMetadata::Source(&metadata))).unwrap();
        assert_eq!(version.version, "0.0.0-dev+g012345678");
        assert_eq!(version.revision, SHA);
        assert_eq!(version.branch, "main");
    }

    #[test]
    fn rejects_noncanonical_or_ambiguous_tags() {
        assert!(parse_release_tag("openvmm-v0.01.0").is_err());
        assert!(parse_release_tag("openvmm-v0.1").is_err());
        assert!(
            resolve_version(
                Some(git(&["openvmm-v0.1.0", "openvmm-v0.2.0"], false)),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_inconsistent_generated_metadata() {
        let metadata = ReleaseMetadata {
            version: "0.12.4".into(),
            tag: "openvmm-v0.12.3".into(),
            revision: SHA.into(),
        };
        assert!(validate_release_metadata(&metadata).is_err());

        let sha256_metadata = ReleaseMetadata {
            version: "0.12.3".into(),
            tag: "openvmm-v0.12.3".into(),
            revision: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        };
        assert!(validate_release_metadata(&sha256_metadata).is_ok());

        let invalid_revision = ReleaseMetadata {
            revision: "not-a-git-revision".into(),
            ..sha256_metadata
        };
        assert!(validate_release_metadata(&invalid_revision).is_err());
    }

    #[test]
    fn ignores_only_the_known_github_ci_config_rewrite() {
        let repo = temporary_repo();
        let config_path = repo.join(".cargo/config.toml");
        std::fs::write(&config_path, "[build]\n rustflags = [\"-Dwarnings\"]\n").unwrap();

        assert!(ci_config_rewrite_is_only_change(&repo, true));
        assert!(!ci_config_rewrite_is_only_change(&repo, false));

        std::fs::write(repo.join("other.txt"), "dirty\n").unwrap();
        assert!(!ci_config_rewrite_is_only_change(&repo, true));
        std::fs::remove_file(repo.join("other.txt")).unwrap();

        std::fs::write(&config_path, "[build]\nrustflags = [\"-Dwarnings\"]\n").unwrap();
        assert!(!ci_config_rewrite_is_only_change(&repo, true));

        std::fs::remove_dir_all(repo).unwrap();
    }
}
