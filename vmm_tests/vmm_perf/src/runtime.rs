// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::config::VmmPerfProfile;
use anyhow::Context as _;
use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const ARCHIVE_SIGNATURE_FILE: &str = ".archive-signature";
const SUPPORTED_ARCHIVE_EXTENSIONS: &[&str] = if cfg!(windows) {
    &[".tar.gz", ".tar", ".zip"]
} else {
    &[".tar.gz", ".tar"]
};

pub(crate) struct VmmPerfRuntime {
    root: PathBuf,
    virtual_client: PathBuf,
}

impl VmmPerfRuntime {
    pub(crate) fn prepare(archive: &Path) -> anyhow::Result<Self> {
        ensure_file(archive, "VMM.Perf runtime archive")?;

        let root = extract_runtime(archive)?;
        ensure_runtime_executables(&root)?;
        let virtual_client = root.join(virtual_client_name());
        Ok(Self {
            root,
            virtual_client,
        })
    }

    pub(crate) fn validate_profile(&self, profile: VmmPerfProfile) -> anyhow::Result<()> {
        ensure_file(
            &self.root.join("profiles").join(profile.file()),
            "VMM.Perf profile",
        )
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn virtual_client(&self) -> &Path {
        &self.virtual_client
    }

    pub(crate) fn package_dir(&self) -> PathBuf {
        self.root.join("packages")
    }

    pub(crate) fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub(crate) fn reset_logs(&self) -> anyhow::Result<()> {
        let logs = self.logs_dir();
        if logs.exists() {
            fs_err::remove_dir_all(logs)?;
        }
        Ok(())
    }
}

fn extract_runtime(archive: &Path) -> anyhow::Result<PathBuf> {
    let archive_parent = archive
        .parent()
        .context("VMM.Perf archive has no parent directory")?;
    let archive_name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .context("VMM.Perf archive filename is not valid UTF-8")?;
    let cache_name = archive_cache_name(archive_name)?;
    let cache_dir = archive_parent.join(format!("{cache_name}-extracted"));
    let archive_signature = archive_signature(archive)?;

    if cache_dir.exists() {
        if fs_err::read_to_string(cache_dir.join(ARCHIVE_SIGNATURE_FILE))
            .is_ok_and(|signature| signature == archive_signature)
            && let Ok(runtime_dir) = find_runtime_dir(&cache_dir)
        {
            return Ok(runtime_dir);
        }
        fs_err::remove_dir_all(&cache_dir)?;
    }

    let staging = tempfile::Builder::new()
        .prefix(".vmm-perf-runtime-extracting-")
        .tempdir_in(archive_parent)
        .context("failed to create VMM.Perf runtime extraction staging directory")?;

    let status = Command::new("tar")
        .args(["-xf"])
        .arg(archive)
        .arg("-C")
        .arg(staging.path())
        .status()
        .context("failed to launch tar for VMM.Perf runtime extraction")?;
    anyhow::ensure!(status.success(), "failed to extract VMM.Perf runtime");
    find_runtime_dir(staging.path())?;
    fs_err::write(
        staging.path().join(ARCHIVE_SIGNATURE_FILE),
        &archive_signature,
    )?;

    match fs_err::rename(staging.path(), &cache_dir) {
        Ok(()) => {}
        Err(err) if cache_dir.exists() => {
            anyhow::ensure!(
                fs_err::read_to_string(cache_dir.join(ARCHIVE_SIGNATURE_FILE))
                    .is_ok_and(|signature| signature == archive_signature),
                "concurrent VMM.Perf runtime extraction used a different archive"
            );
            find_runtime_dir(&cache_dir)
                .context("concurrent VMM.Perf runtime extraction produced an invalid cache")?;
            tracing::debug!(%err, "using concurrently extracted VMM.Perf runtime");
        }
        Err(err) => return Err(err.into()),
    }

    find_runtime_dir(&cache_dir)
}

fn archive_cache_name(archive_name: &str) -> anyhow::Result<&str> {
    SUPPORTED_ARCHIVE_EXTENSIONS
        .iter()
        .find_map(|extension| archive_name.strip_suffix(extension))
        .with_context(|| {
            format!(
                "unsupported VMM.Perf archive format for {archive_name}; expected {}",
                SUPPORTED_ARCHIVE_EXTENSIONS.join(", ")
            )
        })
}

pub(crate) fn archive_signature(archive: &Path) -> anyhow::Result<String> {
    let metadata = fs_err::metadata(archive)?;
    let modified = metadata
        .modified()
        .context("failed to query VMM.Perf archive modification time")?
        .duration_since(std::time::UNIX_EPOCH)
        .context("VMM.Perf archive modification time predates the Unix epoch")?;
    Ok(format!(
        "size={};modified_nanos={}",
        metadata.len(),
        modified.as_nanos()
    ))
}

pub(crate) fn find_runtime_dir(root: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(root.is_dir(), "runtime extraction directory is missing");
    let mut pending = VecDeque::from([(root.to_path_buf(), 0_u8)]);
    let mut candidates = Vec::new();
    while let Some((directory, depth)) = pending.pop_front() {
        if directory.join(virtual_client_name()).is_file() {
            candidates.push(directory.clone());
        }
        if depth == 4 {
            continue;
        }
        for entry in fs_err::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push_back((entry.path(), depth + 1));
            }
        }
    }
    match candidates.as_slice() {
        [runtime_dir] => Ok(runtime_dir.clone()),
        [] => anyhow::bail!(
            "VMM.Perf archive did not contain {} within four directory levels",
            virtual_client_name()
        ),
        _ => anyhow::bail!("VMM.Perf archive contained multiple runtime directories"),
    }
}

fn ensure_runtime_executables(runtime_dir: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            runtime_dir.join(virtual_client_name()),
            runtime_dir.join("cidata-inject"),
        ] {
            if path.is_file() {
                let mut permissions = fs_err::metadata(&path)?.permissions();
                permissions.set_mode(permissions.mode() | 0o111);
                fs_err::set_permissions(path, permissions)?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = runtime_dir;
    Ok(())
}

const fn virtual_client_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "VirtualClient.exe"
    } else {
        "VirtualClient"
    }
}

fn ensure_file(path: &Path, description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path.is_file(),
        "{description} does not exist or is not a file: {}",
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::archive_cache_name;
    use super::archive_signature;
    use super::find_runtime_dir;
    use super::virtual_client_name;
    use crate::test_support;
    use test_with_tracing::test;

    #[test]
    fn derives_cache_names_from_supported_archives() -> anyhow::Result<()> {
        assert_eq!(
            archive_cache_name("vmm-perf-linux-x64.tar.gz")?,
            "vmm-perf-linux-x64"
        );
        assert_eq!(
            archive_cache_name("vmm-perf-linux-x64.tar")?,
            "vmm-perf-linux-x64"
        );
        assert_eq!(
            archive_cache_name("vmm-perf-win-x64.zip").is_ok(),
            cfg!(windows)
        );
        assert!(archive_cache_name("vmm-perf-win-x64.7z").is_err());
        Ok(())
    }

    #[test]
    fn finds_single_runtime_directory() -> anyhow::Result<()> {
        let scratch = test_support::tempdir("runtime-find")?;
        let runtime_dir = scratch.path().join("payload").join("linux-x64");

        fs_err::create_dir_all(&runtime_dir)?;
        fs_err::write(runtime_dir.join(virtual_client_name()), [])?;

        assert_eq!(find_runtime_dir(scratch.path())?, runtime_dir);
        Ok(())
    }

    #[test]
    fn rejects_multiple_runtime_directories() -> anyhow::Result<()> {
        let scratch = test_support::tempdir("runtime-multiple")?;
        for directory in ["a", "b"] {
            let runtime_dir = scratch.path().join(directory);
            fs_err::create_dir_all(&runtime_dir)?;
            fs_err::write(runtime_dir.join(virtual_client_name()), [])?;
        }

        let error = find_runtime_dir(scratch.path()).unwrap_err();
        assert!(format!("{error:#}").contains("multiple runtime directories"));
        Ok(())
    }

    #[test]
    fn archive_signature_changes_with_file_contents() -> anyhow::Result<()> {
        let scratch = test_support::tempdir("runtime-signature")?;
        let archive = scratch.path().join("runtime.tar.gz");

        fs_err::write(&archive, "abc")?;
        let first = archive_signature(&archive)?;
        fs_err::write(&archive, "abcd")?;
        let second = archive_signature(&archive)?;

        assert_ne!(first, second);
        Ok(())
    }
}
