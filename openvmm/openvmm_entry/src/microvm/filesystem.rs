// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM virtio-fs attachment.

use crate::cli_args;
use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

pub(super) const MICROVM_FILESYSTEM_STABLE_ID: &str = "fs:microvm0";

#[derive(Clone)]
pub(super) struct EffectiveMicrovmFilesystem {
    pub(super) config: openvmm_defs::microvm::MicrovmFilesystemConfig,
    pub(super) root_path: String,
    pub(super) attachment: openvmm_helpers::snapshot::microvm::SnapshotAttachment,
}

fn canonical_microvm_filesystem_root(
    path: &Path,
) -> anyhow::Result<(PathBuf, &'static str, Vec<u8>)> {
    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "microVM filesystem host path is empty"
    );
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("failed to resolve current directory for microVM filesystem")?
            .join(path)
    };

    let mut current = PathBuf::new();
    for component in absolute.components() {
        use std::path::Component;
        match component {
            Component::Prefix(_) | Component::RootDir => {
                current.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                anyhow::bail!("microVM filesystem host path contains a parent component")
            }
            Component::Normal(_) => {
                current.push(component.as_os_str());
                let metadata = fs_err::symlink_metadata(&current).with_context(|| {
                    format!(
                        "failed to inspect microVM filesystem path component {}",
                        current.display()
                    )
                })?;
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "microVM filesystem path component is a symbolic link: {}",
                    current.display()
                );
                #[cfg(windows)]
                anyhow::ensure!(
                    std::os::windows::fs::MetadataExt::file_attributes(&metadata) & 0x400 == 0,
                    "microVM filesystem path component is a reparse point: {}",
                    current.display()
                );
            }
        }
    }

    let canonical = fs_err::canonicalize(&absolute).with_context(|| {
        format!(
            "failed to canonicalize microVM filesystem root {}",
            absolute.display()
        )
    })?;
    let metadata = fs_err::symlink_metadata(&canonical).with_context(|| {
        format!(
            "failed to inspect microVM filesystem root {}",
            canonical.display()
        )
    })?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "microVM filesystem root is not a plain directory: {}",
        canonical.display()
    );

    #[cfg(unix)]
    let (identity_kind, identity) = {
        use std::os::unix::fs::MetadataExt as _;
        let mut identity = b"openvmm-microvm-fs-unix-v1\0".to_vec();
        identity.extend_from_slice(&metadata.dev().to_le_bytes());
        identity.extend_from_slice(&metadata.ino().to_le_bytes());
        ("unix-device-inode-v1", identity)
    };
    #[cfg(windows)]
    let (identity_kind, identity) = {
        use std::os::windows::ffi::OsStrExt as _;
        let stat = pal::windows::fs::query_stat_lx_by_name(&canonical)
            .context("failed to query the microVM filesystem root identity")?;
        anyhow::ensure!(
            stat.FileId != 0,
            "microVM filesystem root has no stable file identity"
        );
        let volume = canonical
            .components()
            .next()
            .and_then(|component| match component {
                std::path::Component::Prefix(prefix) => Some(prefix.as_os_str()),
                _ => None,
            })
            .context("microVM filesystem root has no volume prefix")?;
        let volume = volume.encode_wide().collect::<Vec<_>>();
        let volume_bytes = u32::try_from(volume.len())
            .context("microVM filesystem volume identity is too long")?
            .to_le_bytes();
        let mut identity = b"openvmm-microvm-fs-windows-v1\0".to_vec();
        identity.extend_from_slice(&volume_bytes);
        identity.extend(volume.into_iter().flat_map(u16::to_le_bytes));
        identity.extend_from_slice(&stat.FileId.to_le_bytes());
        ("windows-volume-file-id-v1", identity)
    };
    #[cfg(not(any(unix, windows)))]
    let (identity_kind, identity) =
        { anyhow::bail!("microVM virtio-fs requires Linux KVM/MSHV or Windows WHP") };
    anyhow::ensure!(
        !identity.is_empty() && identity.len() <= 4096,
        "microVM filesystem root identity is empty or exceeds 4096 bytes"
    );

    Ok((canonical, identity_kind, identity))
}

pub(crate) fn microvm_filesystem_attachment(
    host_path: &Path,
) -> anyhow::Result<(
    String,
    openvmm_helpers::snapshot::microvm::SnapshotAttachment,
)> {
    let (canonical, identity_kind, identity) = canonical_microvm_filesystem_root(host_path)?;
    let root_path = canonical
        .to_str()
        .context("microVM filesystem host path is not valid UTF-8")?
        .to_owned();
    Ok((
        root_path,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: MICROVM_FILESYSTEM_STABLE_ID.to_owned(),
            kind: "virtio-fs".to_owned(),
            required: true,
            reconnect_policy: "live-revalidate".to_owned(),
            identity_kind: identity_kind.to_owned(),
            identity,
            length: 0,
            reconnect_timeout_ms: 0,
        },
    ))
}

fn microvm_filesystem_from_mount(
    requested: &cli_args::microvm::MicrovmMountCli,
) -> anyhow::Result<EffectiveMicrovmFilesystem> {
    let (root_path, attachment) = microvm_filesystem_attachment(&requested.host_path)?;
    let config = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
        requested.guest_target.clone(),
        requested.access,
    )?;
    Ok(EffectiveMicrovmFilesystem {
        config,
        root_path,
        attachment,
    })
}

pub(crate) fn validate_microvm_filesystem_private_storage(
    root_path: &Path,
    snapshot_destination: Option<&Path>,
    restore_snapshot: Option<&Path>,
    memory_backing_file: Option<&Path>,
) -> anyhow::Result<()> {
    let root_path = fs_err::canonicalize(root_path).with_context(|| {
        format!(
            "failed to canonicalize microVM filesystem export root {}",
            root_path.display()
        )
    })?;
    let ensure_outside = |path: &Path, description: &str| -> anyhow::Result<()> {
        anyhow::ensure!(
            !path.starts_with(&root_path),
            "{description} must be outside the microVM filesystem export root {}",
            root_path.display()
        );
        Ok(())
    };

    if let Some(destination) = snapshot_destination {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs_err::canonicalize(parent).with_context(|| {
            format!(
                "failed to canonicalize snapshot destination parent {}",
                parent.display()
            )
        })?;
        ensure_outside(&parent, "snapshot destination")?;
    }
    if let Some(snapshot) = restore_snapshot {
        let snapshot = fs_err::canonicalize(snapshot).with_context(|| {
            format!(
                "failed to canonicalize restore snapshot {}",
                snapshot.display()
            )
        })?;
        ensure_outside(&snapshot, "restore snapshot")?;
    }
    if let Some(memory) = memory_backing_file {
        let memory = fs_err::canonicalize(memory).with_context(|| {
            format!(
                "failed to canonicalize guest memory backing file {}",
                memory.display()
            )
        })?;
        ensure_outside(&memory, "guest memory backing file")?;
    }
    Ok(())
}

pub(super) fn effective_microvm_filesystem(
    requested: Option<&cli_args::microvm::MicrovmMountCli>,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<Option<EffectiveMicrovmFilesystem>> {
    if restore.is_none() {
        return requested.map(microvm_filesystem_from_mount).transpose();
    }
    anyhow::ensure!(
        requested.is_none(),
        "snapshot does not support restore-time microVM filesystem attachment"
    );
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;
    use crate::microvm::network::tests::network_contract;
    use clap::Parser as _;
    use test_with_tracing::test;

    fn restore_mount_options(root: &Path, mode: &str) -> Options {
        Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--restore-snapshot",
            "snapshot",
            "--mount",
            &format!("/mnt/share,{},{}", root.display(), mode),
        ])
        .unwrap()
    }

    #[test]
    fn filesystem_mount_resolves_the_canonical_root() {
        let root = tempfile::tempdir().unwrap();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--mount",
            &format!("/mnt/share,{},rw", root.path().display()),
        ])
        .unwrap();
        options.validate_microvm_options().unwrap();
        let filesystem = effective_microvm_filesystem(options.microvm.microvm_mount.as_ref(), None)
            .unwrap()
            .unwrap();
        assert_eq!(filesystem.config.guest_mount_target, "/mnt/share");
        assert_eq!(
            filesystem.config.access,
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
        );
        assert_eq!(
            filesystem.root_path,
            fs_err::canonicalize(root.path()).unwrap().to_str().unwrap()
        );
        assert_eq!(
            filesystem.attachment.stable_id,
            MICROVM_FILESYSTEM_STABLE_ID
        );
    }

    #[test]
    fn filesystem_restore_rejects_mount_for_legacy_snapshot_without_slot() {
        let root = tempfile::tempdir().unwrap();
        let contract = network_contract();
        let options = restore_mount_options(root.path(), "ro");
        let error = match effective_microvm_filesystem(
            options.microvm.microvm_mount.as_ref(),
            Some(&contract),
        ) {
            Err(error) => error,
            Ok(_) => panic!("legacy snapshot unexpectedly accepted a restore-time mount"),
        };
        assert!(
            error
                .to_string()
                .contains("does not support restore-time microVM filesystem attachment")
        );
    }

    #[test]
    fn filesystem_root_rejects_parent_components() {
        let root = tempfile::tempdir().unwrap();
        assert!(canonical_microvm_filesystem_root(&root.path().join("child").join("..")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn filesystem_root_rejects_symbolic_link_components() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs_err::create_dir(&target).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(canonical_microvm_filesystem_root(&link).is_err());
    }

    #[test]
    fn filesystem_export_rejects_snapshot_and_memory_storage() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("share");
        fs_err::create_dir(&root).unwrap();
        let memory = root.join("memory.bin");
        fs_err::write(&memory, b"memory").unwrap();
        let restore = root.join("restore");
        fs_err::create_dir(&restore).unwrap();

        assert!(
            validate_microvm_filesystem_private_storage(
                &root,
                Some(&root.join("snapshot")),
                None,
                None,
            )
            .is_err()
        );
        assert!(
            validate_microvm_filesystem_private_storage(&root, None, Some(&restore), None,)
                .is_err()
        );
        assert!(
            validate_microvm_filesystem_private_storage(&root, None, None, Some(&memory),).is_err()
        );
        validate_microvm_filesystem_private_storage(
            &root,
            Some(&parent.path().join("snapshot")),
            None,
            None,
        )
        .unwrap();
    }
}
