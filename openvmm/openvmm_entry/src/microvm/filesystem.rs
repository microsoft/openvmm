// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM virtio-fs attachment and filesystem policy.

use crate::cli_args;
use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

pub(super) const MICROVM_FILESYSTEM_STABLE_ID: &str =
    openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0].stable_id;

#[derive(Clone, Debug)]
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

fn canonical_microvm_filesystem_denied_paths(
    root_path: &Path,
    requested: &[PathBuf],
) -> anyhow::Result<Vec<String>> {
    #[cfg(unix)]
    let root_metadata = fs_err::symlink_metadata(root_path).with_context(|| {
        format!(
            "failed to inspect microVM filesystem root {}",
            root_path.display()
        )
    })?;
    let mut relative_paths = Vec::with_capacity(requested.len());
    for denied in requested {
        anyhow::ensure!(
            !denied.as_os_str().is_empty()
                && !denied.components().any(|component| matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )),
            "microVM denied path is empty or contains a dot or parent component: {}",
            denied.display()
        );
        let absolute = if denied.is_absolute() {
            denied.clone()
        } else {
            root_path.join(denied)
        };
        let canonical = fs_err::canonicalize(&absolute).with_context(|| {
            format!(
                "failed to canonicalize microVM denied path {}",
                absolute.display()
            )
        })?;
        let relative = canonical.strip_prefix(root_path).with_context(|| {
            format!(
                "microVM denied path resolves outside the filesystem export root: {}",
                denied.display()
            )
        })?;
        anyhow::ensure!(
            !relative.as_os_str().is_empty(),
            "microVM denied path cannot hide the complete filesystem export"
        );

        let mut current = root_path.to_owned();
        for component in relative.components() {
            let std::path::Component::Normal(component) = component else {
                anyhow::bail!("microVM denied path contains a non-normal component");
            };
            current.push(component);
            let metadata = fs_err::symlink_metadata(&current).with_context(|| {
                format!(
                    "failed to inspect microVM denied path component {}",
                    current.display()
                )
            })?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "microVM denied path component is a symbolic link: {}",
                current.display()
            );
            #[cfg(windows)]
            anyhow::ensure!(
                std::os::windows::fs::MetadataExt::file_attributes(&metadata) & 0x400 == 0,
                "microVM denied path component is a reparse point: {}",
                current.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let denied_metadata = fs_err::symlink_metadata(&canonical)?;
            anyhow::ensure!(
                denied_metadata.dev() == root_metadata.dev(),
                "microVM denied path crosses a nested mount: {}",
                denied.display()
            );
        }
        relative_paths.push(relative.to_owned());
    }
    relative_paths.sort_unstable();
    for pair in relative_paths.windows(2) {
        anyhow::ensure!(
            pair[0] != pair[1] && !pair[1].starts_with(&pair[0]),
            "microVM denied paths must be unique and non-overlapping"
        );
    }
    let encoded = relative_paths
        .iter()
        .map(|path| {
            path.iter()
                .map(|component| {
                    component
                        .to_str()
                        .context("microVM denied path is not valid UTF-8")
                })
                .collect::<anyhow::Result<Vec<_>>>()
                .map(|components| components.join("/"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    openvmm_defs::microvm::MicrovmFilesystemConfig::new(
        "/unused".to_owned(),
        openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
    )?
    .with_denied_paths(encoded.clone())?;
    Ok(encoded)
}

/// Returns the canonical host root of `host_path` and its live attachment
/// identity in the virtio-fs slot `slot`.
pub(crate) fn microvm_filesystem_attachment(
    host_path: &Path,
    slot: &openvmm_defs::microvm::MicrovmFilesystemSlot,
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
            stable_id: slot.stable_id.to_owned(),
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

/// Builds the filesystem of `requested` in its canonical host root, with the
/// denied paths that `--mount-deny` attributed to it.
fn microvm_filesystem_from_root(
    requested: &cli_args::microvm::MicrovmMountCli,
    (root_path, attachment): (
        String,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    ),
    denied_paths: &[PathBuf],
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
) -> anyhow::Result<EffectiveMicrovmFilesystem> {
    if owner.is_caller() {
        validate_caller_owned_root(Path::new(&root_path))?;
    }
    let denied_paths =
        canonical_microvm_filesystem_denied_paths(Path::new(&root_path), denied_paths)?;
    let config = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
        requested.guest_target.clone(),
        requested.access,
    )?
    .with_denied_paths(denied_paths)?
    .with_owner(owner);
    Ok(EffectiveMicrovmFilesystem {
        config,
        root_path,
        attachment,
    })
}

/// Rejects host roots that overlap, so that no share can reach the files of
/// another share, or the paths that another share denies, under a different
/// access policy. Equal root identities catch one directory reached through
/// two canonical paths, such as a bind mount.
fn validate_microvm_filesystem_roots(
    roots: &[(
        String,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    )],
) -> anyhow::Result<()> {
    for (index, (root, attachment)) in roots.iter().enumerate() {
        for (other, other_attachment) in &roots[..index] {
            anyhow::ensure!(
                !Path::new(root).starts_with(other)
                    && !Path::new(other).starts_with(root)
                    && attachment.identity != other_attachment.identity,
                "microVM filesystem host directories must not overlap: {other} and {root}"
            );
        }
    }
    #[cfg(target_os = "linux")]
    if roots.len() > 1 {
        let mountinfo = fs_err::read_to_string("/proc/self/mountinfo")?;
        let roots = roots
            .iter()
            .map(|(root, _)| Path::new(root))
            .collect::<Vec<_>>();
        mount_sources::validate_disjoint(&mountinfo, &roots)?;
    }
    Ok(())
}

/// Compares where the files of each host root come from.
///
/// A bind mount gives a directory a second canonical path and root identity,
/// so it can expose part of one share as another share's root, or inside
/// another share's tree. Each root reaches the source directory of the mount
/// that contains it and the source of every mount below it, and two roots
/// overlap when any of those ranges of one filesystem contains another.
#[cfg(target_os = "linux")]
mod mount_sources {
    use anyhow::Context;
    use std::path::Path;
    use std::path::PathBuf;

    struct Mount {
        device: String,
        root: PathBuf,
        mount_point: PathBuf,
    }

    /// A directory tree of one filesystem, named by its device and its path
    /// within that filesystem.
    struct Source {
        device: String,
        path: PathBuf,
    }

    impl Source {
        fn overlaps(&self, other: &Source) -> bool {
            self.device == other.device
                && (self.path.starts_with(&other.path) || other.path.starts_with(&self.path))
        }
    }

    fn unescape(field: &str) -> anyhow::Result<PathBuf> {
        use std::os::unix::ffi::OsStringExt as _;
        let bytes = field.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\' {
                let digits = bytes
                    .get(index + 1..index + 4)
                    .and_then(|digits| std::str::from_utf8(digits).ok())
                    .and_then(|digits| u8::from_str_radix(digits, 8).ok())
                    .with_context(|| format!("invalid escape in mountinfo field '{field}'"))?;
                decoded.push(digits);
                index += 4;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)))
    }

    fn parse(mountinfo: &str) -> anyhow::Result<Vec<Mount>> {
        mountinfo
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let fields = line.split(' ').collect::<Vec<_>>();
                anyhow::ensure!(fields.len() >= 5, "malformed mountinfo line '{line}'");
                Ok(Mount {
                    device: fields[2].to_owned(),
                    root: unescape(fields[3])?,
                    mount_point: unescape(fields[4])?,
                })
            })
            .collect()
    }

    fn sources(mounts: &[Mount], root: &Path) -> anyhow::Result<Vec<Source>> {
        // The deepest mount point that contains the root, and among mounts
        // stacked there, the latest, which hides the others.
        let (_, containing) = mounts
            .iter()
            .enumerate()
            .filter(|(_, mount)| root.starts_with(&mount.mount_point))
            .max_by_key(|(index, mount)| (mount.mount_point.components().count(), *index))
            .with_context(|| format!("no mount contains {}", root.display()))?;
        let relative = root.strip_prefix(&containing.mount_point)?;
        let mut sources = vec![Source {
            device: containing.device.clone(),
            path: containing.root.join(relative),
        }];
        sources.extend(
            mounts
                .iter()
                .filter(|mount| mount.mount_point != root && mount.mount_point.starts_with(root))
                .map(|mount| Source {
                    device: mount.device.clone(),
                    path: mount.root.clone(),
                }),
        );
        Ok(sources)
    }

    /// Rejects canonical roots that reach a common directory of a filesystem.
    pub(super) fn validate_disjoint(mountinfo: &str, roots: &[&Path]) -> anyhow::Result<()> {
        let mounts = parse(mountinfo)?;
        let sources = roots
            .iter()
            .map(|root| sources(&mounts, root))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for (index, root_sources) in sources.iter().enumerate() {
            for (other_index, other_sources) in sources[..index].iter().enumerate() {
                anyhow::ensure!(
                    !root_sources
                        .iter()
                        .any(|source| other_sources.iter().any(|other| source.overlaps(other))),
                    "microVM filesystem host directories must not overlap: {} and {} reach the same files through a mount",
                    roots[other_index].display(),
                    roots[index].display()
                );
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::validate_disjoint;
        use std::path::Path;

        const BASE: &str = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
23 22 0:21 / /proc rw,nosuid shared:2 - proc proc rw
";

        fn check(mountinfo: &str, first: &str, second: &str) -> anyhow::Result<()> {
            validate_disjoint(mountinfo, &[Path::new(first), Path::new(second)])
        }

        #[test]
        fn separate_directories_of_one_filesystem_are_disjoint() {
            check(BASE, "/workspace", "/toolcache").unwrap();
            check(BASE, "/work", "/workspace").unwrap();
        }

        #[test]
        fn a_bind_mount_of_a_subdirectory_overlaps_its_source() {
            let mountinfo =
                format!("{BASE}30 22 8:1 /workspace/cache /toolcache rw - ext4 /dev/sda1 rw\n");
            let error = check(&mountinfo, "/workspace", "/toolcache").unwrap_err();
            assert!(error.to_string().contains("through a mount"), "{error:#}");
            check(&mountinfo, "/toolcache", "/workspace").unwrap_err();
            // The bind mount's source is not inside an unrelated directory.
            check(&mountinfo, "/srv", "/toolcache").unwrap();
        }

        #[test]
        fn a_mount_below_one_root_overlaps_the_other_root() {
            let mountinfo =
                format!("{BASE}31 22 8:1 /toolcache /workspace/tools rw - ext4 /dev/sda1 rw\n");
            check(&mountinfo, "/workspace", "/toolcache").unwrap_err();
            check(&mountinfo, "/workspace", "/toolcache/node").unwrap_err();
            check(&mountinfo, "/workspace", "/opt").unwrap();
        }

        #[test]
        fn other_filesystems_and_unrelated_mounts_do_not_overlap() {
            let mountinfo = format!(
                "{BASE}40 22 8:2 / /data rw - ext4 /dev/sda2 rw\n\
                 41 22 0:50 / /workspace/tmp rw - tmpfs tmpfs rw\n"
            );
            check(&mountinfo, "/workspace", "/data").unwrap();
            // A second mount of the same filesystem reaches the same files.
            let twice = format!("{mountinfo}42 22 8:2 / /mnt/data rw - ext4 /dev/sda2 rw\n");
            check(&twice, "/data/a", "/mnt/data/a/b").unwrap_err();
            check(&twice, "/data/a", "/mnt/data/b").unwrap();
        }

        #[test]
        fn escaped_paths_and_stacked_mounts_resolve() {
            let mountinfo = format!(
                "{BASE}50 22 8:1 /src\\040dir /mnt/share\\040one rw - ext4 /dev/sda1 rw\n\
                 51 22 8:2 / /mnt/share\\040one rw - ext4 /dev/sda2 rw\n"
            );
            // The later mount hides the earlier one at the same mount point.
            check(&mountinfo, "/mnt/share one", "/src dir").unwrap();
            check(&mountinfo, "/mnt/share one", "/data").unwrap();
            let bind = format!("{BASE}52 22 8:1 /src\\040dir /mnt/share rw - ext4 /dev/sda1 rw\n");
            check(&bind, "/mnt/share", "/src dir/sub").unwrap_err();
            assert!(validate_disjoint("22 1 8:1", &[Path::new("/a"), Path::new("/b")]).is_err());
        }
    }
}

/// Builds the filesystems of the `--mount` options, which occupy the fixed
/// virtio-fs slots in order. Each `--mount-deny` applies to the export root
/// that contains it; a relative path is relative to the only export root.
fn microvm_filesystems_from_mounts(
    requested: &[cli_args::microvm::MicrovmMountCli],
    denied_paths: &[PathBuf],
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
) -> anyhow::Result<Vec<EffectiveMicrovmFilesystem>> {
    let slots = &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS;
    anyhow::ensure!(
        requested.len() <= slots.len(),
        openvmm_defs::microvm::InvalidMicrovmFilesystemConfig::TooManyFilesystems
    );
    let roots = requested
        .iter()
        .zip(slots)
        .map(|(mount, slot)| microvm_filesystem_attachment(&mount.host_path, slot))
        .collect::<anyhow::Result<Vec<_>>>()?;
    validate_microvm_filesystem_roots(&roots)?;

    let mut attributed = vec![Vec::new(); roots.len()];
    match roots.len() {
        0 => anyhow::ensure!(denied_paths.is_empty(), "--mount-deny requires --mount"),
        1 => attributed[0].extend_from_slice(denied_paths),
        _ => {
            for denied in denied_paths {
                anyhow::ensure!(
                    denied.is_absolute(),
                    "with several --mount options, --mount-deny requires an absolute host path: {}",
                    denied.display()
                );
                let canonical = fs_err::canonicalize(denied).with_context(|| {
                    format!(
                        "failed to canonicalize microVM denied path {}",
                        denied.display()
                    )
                })?;
                let index = roots
                    .iter()
                    .position(|(root, _)| canonical.starts_with(root))
                    .with_context(|| {
                        format!(
                            "microVM denied path resolves outside the filesystem export roots: {}",
                            denied.display()
                        )
                    })?;
                attributed[index].push(denied.clone());
            }
        }
    }

    let filesystems = requested
        .iter()
        .zip(roots)
        .zip(&attributed)
        .map(|((mount, root), denied)| microvm_filesystem_from_root(mount, root, denied, owner))
        .collect::<anyhow::Result<Vec<_>>>()?;
    openvmm_defs::microvm::validate_microvm_filesystems(
        &filesystems
            .iter()
            .map(|filesystem| filesystem.config.clone())
            .collect::<Vec<_>>(),
    )?;
    Ok(filesystems)
}

/// Rejects an export root that caller ownership cannot squash guest root to.
fn validate_caller_owned_root(root_path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = fs_err::symlink_metadata(root_path)?;
        anyhow::ensure!(
            metadata.uid() != 0 && metadata.gid() != 0,
            "--mount-owner caller squashes guest root to the owner of the export root, so {} must not be owned by UID 0 or GID 0",
            root_path.display()
        );
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = root_path;
        anyhow::bail!("--mount-owner caller requires a Linux host")
    }
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

pub(crate) fn microvm_filesystem_from_snapshot(
    saved: &openvmm_helpers::snapshot::microvm::SnapshotMicrovmFilesystem,
) -> anyhow::Result<openvmm_defs::microvm::MicrovmFilesystemConfig> {
    let access = match saved.access_mode.as_str() {
        "ro" => openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
        "rw" => openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite,
        mode => anyhow::bail!("snapshot microVM filesystem access mode '{mode}' is unsupported"),
    };
    openvmm_defs::microvm::MicrovmFilesystemConfig::new(saved.guest_mount_target.clone(), access)?
        .with_denied_paths(saved.denied_paths.clone())
        .context("snapshot microVM filesystem policy is invalid")
        .and_then(|config| {
            Ok(config.with_owner(
                openvmm_helpers::snapshot::microvm::snapshot_microvm_filesystem_owner(
                    &saved.owner_mode,
                )?,
            ))
        })
}

pub(crate) fn microvm_filesystem_slot_from_snapshot(
    contract: &openvmm_helpers::snapshot::microvm::SnapshotMachineContract,
) -> anyhow::Result<bool> {
    let has_device = contract
        .devices
        .iter()
        .any(|device| device.stable_id == MICROVM_FILESYSTEM_STABLE_ID);
    match contract.microvm_filesystem_slot_version {
        0 => Ok(has_device),
        openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION => {
            anyhow::ensure!(
                has_device,
                "snapshot advertises a restore-attachable microVM filesystem slot but omits its fixed device"
            );
            Ok(true)
        }
        version => anyhow::bail!(
            "snapshot microVM filesystem slot capability version {version} is unsupported"
        ),
    }
}

/// Returns the effective filesystems of the `--mount` options, in virtio-fs
/// slot order. A restore must supply exactly the snapshot's filesystems, in
/// the same order, unless the snapshot's first slot is dormant, in which case
/// one `--mount` may bind a new filesystem to it.
pub(super) fn effective_microvm_filesystems(
    requested: &[cli_args::microvm::MicrovmMountCli],
    denied_paths: &[PathBuf],
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<Vec<EffectiveMicrovmFilesystem>> {
    let Some(restore) = restore else {
        return microvm_filesystems_from_mounts(requested, denied_paths, owner);
    };

    let has_device = microvm_filesystem_slot_from_snapshot(restore)?;
    let saved_attachment = restore
        .attachments
        .iter()
        .find(|attachment| attachment.stable_id == MICROVM_FILESYSTEM_STABLE_ID);
    anyhow::ensure!(
        saved_attachment.is_some() == restore.microvm_filesystem.is_some()
            && (restore.microvm_filesystem_slot_version
                == openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION
                || has_device == restore.microvm_filesystem.is_some()),
        "snapshot microVM filesystem slot, policy, and attachment inventories disagree"
    );
    let saved = restore.microvm_filesystems().collect::<Vec<_>>();
    if saved.is_empty() {
        if requested.is_empty() {
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            restore.microvm_filesystem_slot_version
                == openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION,
            "snapshot does not support restore-time microVM filesystem attachment"
        );
        // Only the dormant first slot exists in the snapshot's machine.
        anyhow::ensure!(
            requested.len() == 1,
            "a dormant-slot snapshot can attach only one --mount on restore"
        );
        return microvm_filesystems_from_mounts(requested, denied_paths, owner);
    }
    anyhow::ensure!(
        !requested.is_empty(),
        "snapshot restore requires a fresh --mount attachment for fs:microvm0"
    );
    anyhow::ensure!(
        requested.len() == saved.len(),
        "snapshot restore requires {} --mount attachments in snapshot order, but {} were supplied",
        saved.len(),
        requested.len()
    );
    let mut configs = Vec::with_capacity(saved.len());
    for (requested, saved) in requested.iter().zip(&saved) {
        let config = microvm_filesystem_from_snapshot(saved)?;
        anyhow::ensure!(
            requested.guest_target == config.guest_mount_target
                && requested.access == config.access,
            "restore-time mount target or access mode does not match the snapshot contract"
        );
        anyhow::ensure!(
            owner == config.owner,
            "restore-time --mount-owner {} does not match the snapshot contract ({})",
            owner.as_str(),
            config.owner.as_str()
        );
        configs.push(config);
    }
    let effective = microvm_filesystems_from_mounts(requested, denied_paths, owner)?;
    for ((effective, saved), config) in effective.iter().zip(&saved).zip(&configs) {
        anyhow::ensure!(
            !saved.canonical_host_path.is_empty(),
            "snapshot filesystem canonical host path is missing; this snapshot predates path-bound filesystem restore"
        );
        anyhow::ensure!(
            effective.root_path == saved.canonical_host_path,
            "restore-time filesystem canonical host path does not match the snapshot contract"
        );
        anyhow::ensure!(
            restore.attachments.contains(&effective.attachment),
            "restore-time filesystem root identity does not match the snapshot attachment"
        );
        anyhow::ensure!(
            effective.config == *config,
            "restore-time filesystem denied paths do not match the snapshot contract"
        );
    }
    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;
    use crate::microvm::network::tests::network_contract;
    use clap::Parser as _;
    use openvmm_defs::microvm::build_microvm_command_line;
    use test_with_tracing::test;

    const VMM: openvmm_defs::microvm::MicrovmFilesystemOwner =
        openvmm_defs::microvm::MicrovmFilesystemOwner::Vmm;

    /// Resolves at most one `--mount`, like the single-share tests expect.
    fn effective_microvm_filesystem(
        requested: &[cli_args::microvm::MicrovmMountCli],
        denied_paths: &[PathBuf],
        owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
        restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
    ) -> anyhow::Result<Option<EffectiveMicrovmFilesystem>> {
        let mut filesystems =
            effective_microvm_filesystems(requested, denied_paths, owner, restore)?;
        assert!(filesystems.len() <= 1);
        Ok(filesystems.pop())
    }

    fn filesystem_contract(
        root: &Path,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        filesystem_contract_with_owner(root, VMM)
    }

    fn filesystem_contract_with_owner(
        root: &Path,
        owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let filesystem = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            "/mnt/share".to_owned(),
            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
        )
        .unwrap()
        .with_owner(owner);
        filesystems_contract(&[(filesystem, root)])
    }

    fn dormant_filesystem_contract() -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract
    {
        filesystems_contract(&[])
    }

    /// Builds the contract of a cold-booted machine with `shares` attached to
    /// the virtio-fs slots in order, or with a dormant first slot.
    fn filesystems_contract(
        shares: &[(openvmm_defs::microvm::MicrovmFilesystemConfig, &Path)],
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let filesystems = shares
            .iter()
            .map(|(filesystem, _)| filesystem.clone())
            .collect::<Vec<_>>();
        let roots = shares
            .iter()
            .zip(&openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS)
            .map(|((_, root), slot)| microvm_filesystem_attachment(root, slot).unwrap())
            .collect::<Vec<_>>();
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            true,
            &filesystems,
            false,
            false,
            &[],
        )
        .unwrap();
        let mut state_unit_names = [
            "partition",
            "vmtime",
            "pic",
            "ioapic",
            "pit",
            "rtc",
            "microvm-portb",
            "microvm-shutdown",
            "microvm-snapshot-request",
        ]
        .map(str::to_owned)
        .to_vec();
        state_unit_names.extend(
            openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS
                .iter()
                .take(shares.len().max(1))
                .map(|slot| format!("virtiofs-{}", slot.mmio_base)),
        );
        openvmm_helpers::snapshot::microvm::microvm_machine_contract(
            if cfg!(windows) { "whp" } else { "kvm" },
            openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            true,
            filesystems
                .iter()
                .zip(&roots)
                .map(|(filesystem, (root_path, attachment))| {
                    (filesystem, Path::new(root_path), attachment.clone())
                })
                .collect(),
            None,
            None,
            Vec::new(),
            1,
            1024,
            None,
            state_unit_names,
            crate::microvm::restore::tests::test_time_contract(),
            crate::microvm::restore::tests::test_cpu_profile(),
        )
        .unwrap()
    }

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
    fn filesystem_denied_paths_are_canonical_and_root_scoped() {
        let root = tempfile::tempdir().unwrap();
        let secrets = root.path().join("secrets");
        std::fs::create_dir(&secrets).unwrap();
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--mount",
            &format!("/mnt/share,{},rw", root.path().display()),
            "--mount-deny",
            secrets.to_str().unwrap(),
        ])
        .unwrap();
        options.validate_microvm_options().unwrap();
        let filesystem = effective_microvm_filesystem(
            options.microvm.microvm_mount.as_slice(),
            &options.microvm.microvm_mount_deny,
            VMM,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(filesystem.config.denied_paths, vec!["secrets".to_owned()]);

        let outside = tempfile::tempdir().unwrap();
        assert!(
            canonical_microvm_filesystem_denied_paths(root.path(), &[outside.path().to_owned()])
                .is_err()
        );
        assert!(
            canonical_microvm_filesystem_denied_paths(root.path(), &[secrets.clone(), secrets])
                .is_err()
        );
    }

    #[test]
    fn filesystem_restore_requires_same_live_root_identity() {
        let root = tempfile::tempdir().unwrap();
        let contract = filesystem_contract(root.path());
        let options = restore_mount_options(root.path(), "ro");
        let restored = effective_microvm_filesystem(
            options.microvm.microvm_mount.as_slice(),
            &[],
            VMM,
            Some(&contract),
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored.config.guest_mount_target, "/mnt/share");
        assert_eq!(restored.attachment, contract.attachments[0]);

        let replacement = tempfile::tempdir().unwrap();
        let replacement_options = restore_mount_options(replacement.path(), "ro");
        assert!(
            effective_microvm_filesystem(
                replacement_options.microvm.microvm_mount.as_slice(),
                &[],
                VMM,
                Some(&contract)
            )
            .is_err()
        );
    }

    #[test]
    fn filesystem_restore_rejects_same_root_at_a_new_path() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("original");
        let moved = parent.path().join("moved");
        fs_err::create_dir(&original).unwrap();
        let contract = filesystem_contract(&original);
        fs_err::rename(&original, &moved).unwrap();

        let options = restore_mount_options(&moved, "ro");
        assert!(
            effective_microvm_filesystem(
                options.microvm.microvm_mount.as_slice(),
                &[],
                VMM,
                Some(&contract)
            )
            .is_err()
        );
    }

    #[test]
    fn filesystem_restore_rejects_missing_or_changed_policy() {
        let root = tempfile::tempdir().unwrap();
        let contract = filesystem_contract(root.path());
        assert!(effective_microvm_filesystem(&[], &[], VMM, Some(&contract)).is_err());

        let changed_mode = restore_mount_options(root.path(), "rw");
        assert!(
            effective_microvm_filesystem(
                changed_mode.microvm.microvm_mount.as_slice(),
                &[],
                VMM,
                Some(&contract),
            )
            .is_err()
        );
    }

    #[test]
    fn filesystem_restore_without_mount_preserves_dormant_slot() {
        let contract = dormant_filesystem_contract();
        assert!(
            effective_microvm_filesystem(&[], &[], VMM, Some(&contract))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn filesystem_restore_attaches_mount_to_dormant_slot() {
        let root = tempfile::tempdir().unwrap();
        let contract = dormant_filesystem_contract();
        let options = restore_mount_options(root.path(), "rw");
        let filesystem = effective_microvm_filesystem(
            options.microvm.microvm_mount.as_slice(),
            &[],
            VMM,
            Some(&contract),
        )
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
    }

    #[test]
    fn filesystem_restore_rejects_mount_for_legacy_snapshot_without_slot() {
        let root = tempfile::tempdir().unwrap();
        let contract = network_contract();
        let options = restore_mount_options(root.path(), "ro");
        let error = match effective_microvm_filesystem(
            options.microvm.microvm_mount.as_slice(),
            &[],
            VMM,
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

    #[test]
    fn mount_owner_defaults_to_the_vmm_and_requires_a_mount() {
        let root = tempfile::tempdir().unwrap();
        let mount = format!("/mnt/share,{},rw", root.path().display());
        let parse = |extra: &[&str]| {
            Options::try_parse_from(
                ["openvmm", "--machine", "microvm"]
                    .into_iter()
                    .chain(extra.iter().copied()),
            )
        };
        let options = parse(&["--mount", &mount]).unwrap();
        assert_eq!(options.microvm.microvm_mount_owner, None);
        let options = parse(&["--mount", &mount, "--mount-owner", "vmm"]).unwrap();
        assert_eq!(
            options.microvm.microvm_mount_owner,
            Some(cli_args::microvm::MicrovmMountOwnerCli::Vmm)
        );
        options.validate_microvm_options().unwrap();
        assert!(parse(&["--mount-owner", "caller"]).is_err());
        assert!(parse(&["--mount", &mount, "--mount-owner", "root"]).is_err());

        let options = parse(&["--mount", &mount, "--mount-owner", "caller"]).unwrap();
        let validation = options.validate_microvm_options();
        if cfg!(target_os = "linux") {
            validation.unwrap();
        } else {
            assert!(
                validation
                    .unwrap_err()
                    .to_string()
                    .contains("--mount-owner caller requires a Linux host")
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn caller_owner_rejects_a_root_owned_export_root() {
        use std::os::unix::fs::MetadataExt as _;
        let caller = openvmm_defs::microvm::MicrovmFilesystemOwner::Caller;
        let root_owned = Path::new("/");
        let metadata = fs_err::metadata(root_owned).unwrap();
        if metadata.uid() == 0 && metadata.gid() == 0 {
            let options = restore_mount_options(root_owned, "ro");
            let error = match effective_microvm_filesystem(
                options.microvm.microvm_mount.as_slice(),
                &[],
                caller,
                None,
            ) {
                Err(error) => error,
                Ok(_) => panic!("a root-owned export root was accepted for caller ownership"),
            };
            assert!(
                error
                    .to_string()
                    .contains("must not be owned by UID 0 or GID 0")
            );
        }

        let root = tempfile::tempdir().unwrap();
        let metadata = fs_err::metadata(root.path()).unwrap();
        if metadata.uid() != 0 && metadata.gid() != 0 {
            let options = restore_mount_options(root.path(), "rw");
            let filesystem = effective_microvm_filesystem(
                options.microvm.microvm_mount.as_slice(),
                &[],
                caller,
                None,
            )
            .unwrap()
            .unwrap();
            assert_eq!(filesystem.config.owner, caller);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn filesystem_restore_requires_the_same_owner_mode() {
        use std::os::unix::fs::MetadataExt as _;
        let caller = openvmm_defs::microvm::MicrovmFilesystemOwner::Caller;
        let root = tempfile::tempdir().unwrap();
        let metadata = fs_err::metadata(root.path()).unwrap();
        if metadata.uid() == 0 || metadata.gid() == 0 {
            return;
        }
        let options = restore_mount_options(root.path(), "ro");
        let requested = options.microvm.microvm_mount.as_slice();

        let contract = filesystem_contract_with_owner(root.path(), caller);
        assert_eq!(
            contract.microvm_filesystem.as_ref().unwrap().owner_mode,
            "caller"
        );
        let restored = effective_microvm_filesystem(requested, &[], caller, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored.config.owner, caller);
        let error = match effective_microvm_filesystem(requested, &[], VMM, Some(&contract)) {
            Err(error) => error,
            Ok(_) => panic!("restore accepted a different ownership mode"),
        };
        assert!(error.to_string().contains("--mount-owner vmm"));

        // Snapshots record VMM ownership as an empty mode, like those that
        // predate the field.
        let contract = filesystem_contract(root.path());
        assert_eq!(contract.microvm_filesystem.as_ref().unwrap().owner_mode, "");
        assert!(effective_microvm_filesystem(requested, &[], caller, Some(&contract)).is_err());
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

    fn mount_options(mounts: &[String], extra: &[&str]) -> Options {
        let mut args = vec!["openvmm", "--machine", "microvm"];
        for mount in mounts {
            args.extend(["--mount", mount.as_str()]);
        }
        args.extend(extra);
        Options::try_parse_from(args).unwrap()
    }

    fn workspace_and_toolcache(
        workspace: &Path,
        toolcache: &Path,
        toolcache_mode: &str,
    ) -> Vec<String> {
        vec![
            format!("/workspace,{},rw", workspace.display()),
            format!(
                "/opt/hostedtoolcache,{},{toolcache_mode}",
                toolcache.display()
            ),
        ]
    }

    #[test]
    fn mounts_occupy_the_fixed_slots_in_order() {
        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let options = mount_options(
            &workspace_and_toolcache(workspace.path(), toolcache.path(), "ro"),
            &[],
        );
        options.validate_microvm_options().unwrap();
        let filesystems =
            effective_microvm_filesystems(&options.microvm.microvm_mount, &[], VMM, None).unwrap();
        let summary = filesystems
            .iter()
            .map(|filesystem| {
                (
                    filesystem.attachment.stable_id.as_str(),
                    filesystem.config.guest_mount_target.as_str(),
                    filesystem.config.access,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                (
                    "fs:microvm0",
                    "/workspace",
                    openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
                ),
                (
                    "fs:microvm1",
                    "/opt/hostedtoolcache",
                    openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly
                ),
            ]
        );
        for (filesystem, root) in filesystems.iter().zip([&workspace, &toolcache]) {
            assert_eq!(
                filesystem.root_path,
                fs_err::canonicalize(root.path()).unwrap().to_str().unwrap()
            );
        }
        assert_ne!(
            filesystems[0].attachment.identity,
            filesystems[1].attachment.identity
        );
    }

    #[test]
    fn mount_deny_applies_to_the_share_that_contains_it() {
        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let workspace_secrets = workspace.path().join("secrets");
        let toolcache_secrets = toolcache.path().join("credentials");
        fs_err::create_dir(&workspace_secrets).unwrap();
        fs_err::create_dir(&toolcache_secrets).unwrap();
        let mounts = workspace_and_toolcache(workspace.path(), toolcache.path(), "ro");
        let filesystems = effective_microvm_filesystems(
            &mount_options(&mounts, &[]).microvm.microvm_mount,
            &[toolcache_secrets.clone(), workspace_secrets.clone()],
            VMM,
            None,
        )
        .unwrap();
        assert_eq!(filesystems[0].config.denied_paths, ["secrets"]);
        assert_eq!(filesystems[1].config.denied_paths, ["credentials"]);

        // With several shares, a relative path names no export root.
        let error = effective_microvm_filesystems(
            &mount_options(&mounts, &[]).microvm.microvm_mount,
            &[PathBuf::from("secrets")],
            VMM,
            None,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("absolute host path"),
            "{error:#}"
        );

        let outside = tempfile::tempdir().unwrap();
        let error = effective_microvm_filesystems(
            &mount_options(&mounts, &[]).microvm.microvm_mount,
            &[outside.path().to_owned()],
            VMM,
            None,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the filesystem export roots")
        );
    }

    #[test]
    fn mounts_reject_overlapping_host_directories_and_guest_targets() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("nested");
        fs_err::create_dir(&nested).unwrap();
        for (first, second) in [
            (parent.path(), nested.as_path()),
            (nested.as_path(), parent.path()),
            (parent.path(), parent.path()),
        ] {
            let options = mount_options(&workspace_and_toolcache(first, second, "ro"), &[]);
            let error =
                match effective_microvm_filesystems(&options.microvm.microvm_mount, &[], VMM, None)
                {
                    Err(error) => error,
                    Ok(_) => panic!("overlapping host directories were accepted"),
                };
            assert!(error.to_string().contains("must not overlap"), "{error:#}");
        }

        let other = tempfile::tempdir().unwrap();
        let options = mount_options(
            &[
                format!("/workspace,{},rw", parent.path().display()),
                format!("/workspace/cache,{},ro", other.path().display()),
            ],
            &[],
        );
        assert!(options.validate_microvm_options().is_err());
        assert!(
            effective_microvm_filesystems(&options.microvm.microvm_mount, &[], VMM, None).is_err()
        );
    }

    fn two_share_contract(
        workspace: &Path,
        toolcache: &Path,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        filesystems_contract(&[
            (
                openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                    "/workspace".to_owned(),
                    openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite,
                )
                .unwrap(),
                workspace,
            ),
            (
                openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                    "/opt/hostedtoolcache".to_owned(),
                    openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly,
                )
                .unwrap(),
                toolcache,
            ),
        ])
    }

    #[test]
    fn filesystem_restore_requires_every_share_in_snapshot_order() {
        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let contract = two_share_contract(workspace.path(), toolcache.path());
        assert_eq!(contract.microvm_additional_filesystems.len(), 1);
        let restore = |mounts: &[String]| {
            let options = mount_options(mounts, &["--restore-snapshot", "snapshot"]);
            effective_microvm_filesystems(&options.microvm.microvm_mount, &[], VMM, Some(&contract))
        };

        let mounts = workspace_and_toolcache(workspace.path(), toolcache.path(), "ro");
        let restored = restore(&mounts).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(
            restored
                .iter()
                .map(|filesystem| filesystem.attachment.clone())
                .collect::<Vec<_>>(),
            contract.attachments
        );

        let error = restore(&[]).unwrap_err();
        assert!(error.to_string().contains("requires a fresh --mount"));
        let error = restore(&mounts[..1]).unwrap_err();
        assert!(
            error.to_string().contains("2 --mount attachments"),
            "{error:#}"
        );
        let swapped = [mounts[1].clone(), mounts[0].clone()];
        assert!(restore(&swapped).is_err());
        let writable = workspace_and_toolcache(workspace.path(), toolcache.path(), "rw");
        assert!(restore(&writable).is_err());

        let replacement = tempfile::tempdir().unwrap();
        let replaced = workspace_and_toolcache(workspace.path(), replacement.path(), "ro");
        let error = restore(&replaced).unwrap_err();
        assert!(
            error.to_string().contains("canonical host path"),
            "{error:#}"
        );
    }

    #[test]
    fn dormant_snapshot_attaches_only_one_mount_on_restore() {
        let contract = dormant_filesystem_contract();
        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let options = mount_options(
            &workspace_and_toolcache(workspace.path(), toolcache.path(), "ro"),
            &["--restore-snapshot", "snapshot"],
        );
        let error = match effective_microvm_filesystems(
            &options.microvm.microvm_mount,
            &[],
            VMM,
            Some(&contract),
        ) {
            Err(error) => error,
            Ok(_) => panic!("a dormant-slot snapshot attached two shares"),
        };
        assert!(error.to_string().contains("only one --mount"));
    }
}
