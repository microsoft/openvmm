// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

const RELEASE_TAG_PREFIX: &str = "openvmm-v";

pub struct ReleaseMetadata {
    pub version: String,
    pub tag: String,
    pub revision: String,
}

pub struct GitSource {
    pub revision: String,
    pub release_tag: Option<String>,
    pub dirty: bool,
}

pub struct VersionInfo {
    pub product_version: String,
    pub version: String,
    pub revision: String,
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
        return Err("OpenVMM revision must be a full hexadecimal Git object ID".into());
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

fn select_release_tag(tags: Vec<String>) -> Result<Option<String>, String> {
    match tags.as_slice() {
        [] => Ok(None),
        [tag] => {
            parse_release_tag(tag)?;
            Ok(Some(tag.clone()))
        }
        _ => Err(format!(
            "multiple OpenVMM release tags point at HEAD: {tags:?}"
        )),
    }
}

fn git_command(repo: &std::path::Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
}

// This collector is intentionally OpenVMM-specific. The shared
// build_rs_git_info helper remains a compatibility emitter for its existing
// revision and branch variables.
fn git_output(repo: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = git_command(repo, args).map_err(|error| format!("failed to run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout)
        .map(|output| output.trim().to_owned())
        .map_err(|error| format!("git {args:?} returned non-UTF8 output: {error}"))
}

fn ci_config_rewrite_is_only_change(repo: &std::path::Path, github_actions: bool) -> bool {
    if !github_actions {
        return false;
    }

    let config_path = repo.join(".cargo/config.toml");
    let Ok(current) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(committed) = git_command(repo, &["show", "HEAD:.cargo/config.toml"]) else {
        return false;
    };
    if !committed.status.success()
        || current != String::from_utf8_lossy(&committed.stdout).replace("### ENABLE_IN_CI", "")
    {
        return false;
    }

    let Ok(other_changes) = git_command(
        repo,
        &[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--",
            ".",
            ":(exclude).cargo/config.toml",
        ],
    ) else {
        return false;
    };
    other_changes.status.success() && other_changes.stdout.is_empty()
}

pub fn collect_git_source(
    repo: &std::path::Path,
    github_actions: bool,
) -> Result<Option<GitSource>, String> {
    let Ok(prefix) = git_command(repo, &["rev-parse", "--show-prefix"]) else {
        return Ok(None);
    };
    if !prefix.status.success() {
        return Ok(None);
    }
    let prefix = String::from_utf8(prefix.stdout).map_err(|error| {
        format!("git rev-parse --show-prefix returned non-UTF8 output: {error}")
    })?;
    if !prefix.trim().is_empty() {
        return Ok(None);
    }

    let revision = git_output(repo, &["rev-parse", "HEAD"])?;
    validate_revision(&revision)?;
    let tag_glob = format!("{RELEASE_TAG_PREFIX}*");
    let tags = git_output(repo, &["tag", "--points-at", "HEAD", "--list", &tag_glob])?
        .lines()
        .map(str::to_owned)
        .collect();
    let release_tag = select_release_tag(tags)?;
    let status = git_command(repo, &["status", "--porcelain", "--untracked-files=normal"])
        .map_err(|error| format!("failed to run git: {error}"))?;
    if !status.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&status.stderr)
        ));
    }
    let dirty =
        !status.stdout.is_empty() && !ci_config_rewrite_is_only_change(repo, github_actions);

    Ok(Some(GitSource {
        revision,
        release_tag,
        dirty,
    }))
}

pub fn resolve_version(
    git: Option<&GitSource>,
    metadata: Option<&ReleaseMetadata>,
) -> Result<VersionInfo, String> {
    if let Some(git) = git {
        if let Some(tag) = &git.release_tag {
            let product_version = parse_release_tag(tag)?.to_owned();
            let version = if git.dirty {
                format!("{product_version}+dirty")
            } else {
                product_version.clone()
            };
            return Ok(VersionInfo {
                product_version,
                version,
                revision: git.revision.clone(),
            });
        }
    }

    if let Some(git) = git {
        let revision = git
            .revision
            .get(..9)
            .ok_or_else(|| format!("OpenVMM Git revision is too short: {:?}", git.revision))?;
        let dirty = if git.dirty { ".dirty" } else { "" };
        return Ok(VersionInfo {
            product_version: "0.0.0".into(),
            version: format!("0.0.0-dev+g{revision}{dirty}"),
            revision: git.revision.clone(),
        });
    }

    if let Some(metadata) = metadata {
        validate_release_metadata(metadata)?;
        return Ok(VersionInfo {
            product_version: metadata.version.clone(),
            version: metadata.version.clone(),
            revision: metadata.revision.clone(),
        });
    }

    Ok(VersionInfo {
        product_version: "0.0.0".into(),
        version: "0.0.0-dev".into(),
        revision: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn git(tag: Option<&str>, dirty: bool) -> GitSource {
        GitSource {
            revision: SHA.into(),
            release_tag: tag.map(str::to_owned),
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
        let repo = (0..100)
            .find_map(|attempt| {
                let repo = std::env::temp_dir().join(format!(
                    "openvmm-version-info-{}-{nonce}-{attempt}",
                    std::process::id()
                ));
                match std::fs::create_dir(&repo) {
                    Ok(()) => Some(repo),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                    Err(error) => panic!("failed to create {}: {error}", repo.display()),
                }
            })
            .expect("failed to create a unique temporary repository");
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
    fn resolves_release_and_development_versions() {
        let release = git(Some("openvmm-v0.12.3"), false);
        let version = resolve_version(Some(&release), None).unwrap();
        assert_eq!(version.product_version, "0.12.3");
        assert_eq!(version.version, "0.12.3");
        assert_eq!(version.revision, SHA);

        let dirty_release = git(Some("openvmm-v0.12.3"), true);
        assert_eq!(
            resolve_version(Some(&dirty_release), None).unwrap().version,
            "0.12.3+dirty"
        );

        let development = git(None, true);
        assert_eq!(
            resolve_version(Some(&development), None).unwrap().version,
            "0.0.0-dev+g012345678.dirty"
        );
    }

    #[test]
    fn generated_metadata_restores_release_identity() {
        let metadata = ReleaseMetadata {
            version: "0.12.3".into(),
            tag: "openvmm-v0.12.3".into(),
            revision: SHA.into(),
        };
        let version = resolve_version(None, Some(&metadata)).unwrap();
        assert_eq!(version.version, "0.12.3");
        assert_eq!(version.revision, SHA);

        let development = git(None, false);
        assert_eq!(
            resolve_version(Some(&development), Some(&metadata))
                .unwrap()
                .version,
            "0.0.0-dev+g012345678"
        );
    }

    #[test]
    fn rejects_invalid_or_ambiguous_release_identity() {
        assert!(parse_release_tag("openvmm-v0.01.0").is_err());
        assert!(parse_release_tag("openvmm-v0.1").is_err());
        assert!(
            select_release_tag(vec!["openvmm-v0.1.0".into(), "openvmm-v0.2.0".into()]).is_err()
        );

        let metadata = ReleaseMetadata {
            version: "0.12.4".into(),
            tag: "openvmm-v0.12.3".into(),
            revision: SHA.into(),
        };
        assert!(validate_release_metadata(&metadata).is_err());
    }

    #[test]
    fn falls_back_without_git_or_release_metadata() {
        let version = resolve_version(None, None).unwrap();
        assert_eq!(version.version, "0.0.0-dev");
        assert!(version.revision.is_empty());
    }

    #[test]
    fn collects_only_repository_root_git_identity() {
        let repo = temporary_repo();
        run(&repo, &["tag", "openvmm-v0.12.3"]);

        let source = collect_git_source(&repo, false).unwrap().unwrap();
        assert_eq!(source.release_tag.as_deref(), Some("openvmm-v0.12.3"));
        assert!(!source.dirty);

        let nested = repo.join("vendored-openvmm");
        std::fs::create_dir(&nested).unwrap();
        assert!(collect_git_source(&nested, false).unwrap().is_none());

        std::fs::remove_dir_all(repo).unwrap();
    }

    #[test]
    fn ignores_only_the_known_github_ci_config_rewrite() {
        let repo = temporary_repo();
        let config_path = repo.join(".cargo/config.toml");
        std::fs::write(&config_path, "[build]\n rustflags = [\"-Dwarnings\"]\n").unwrap();

        assert!(!collect_git_source(&repo, true).unwrap().unwrap().dirty);
        assert!(collect_git_source(&repo, false).unwrap().unwrap().dirty);

        std::fs::write(repo.join("other.txt"), "dirty\n").unwrap();
        assert!(collect_git_source(&repo, true).unwrap().unwrap().dirty);

        std::fs::remove_dir_all(repo).unwrap();
    }
}
