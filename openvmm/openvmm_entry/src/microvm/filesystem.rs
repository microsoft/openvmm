// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM virtio-fs attachment and filesystem policy.

use crate::cli_args;
use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;

pub(super) const MICROVM_FILESYSTEM_STABLE_ID: &str =
    openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0].stable_id;

/// The largest combined size, in bytes, of the canonical host directories of
/// an aggregate's children, which bounds the aggregate's snapshot contract.
const MAX_AGGREGATE_ROOT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(super) struct EffectiveMicrovmFilesystem {
    pub(super) config: openvmm_defs::microvm::MicrovmFilesystemConfig,
    /// The canonical host root of a single directory; empty for an aggregate.
    pub(super) root_path: String,
    /// The live attachment of the filesystem's device.
    pub(super) attachment: openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    /// The canonical host roots and live root attachments of an aggregate's
    /// children, in child order.
    pub(super) children: Vec<(
        String,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    )>,
}

/// The microVM filesystem that the command line requests.
#[derive(Clone, Copy, Debug)]
pub(super) enum MicrovmFilesystemRequest<'a> {
    /// One host directory, from `--mount`.
    Single(&'a cli_args::microvm::MicrovmMountCli),
    /// Several host directories, from `--mount-aggregate` and `--mount-child`.
    Aggregate {
        guest_target: &'a str,
        children: &'a [cli_args::microvm::MicrovmMountChildCli],
    },
}

impl<'a> MicrovmFilesystemRequest<'a> {
    /// Returns the filesystem that `options` requests, if any.
    pub(super) fn from_options(options: &'a crate::Options) -> Option<Self> {
        if let Some(mount) = &options.microvm.microvm_mount {
            Some(Self::Single(mount))
        } else {
            options
                .microvm
                .microvm_mount_aggregate
                .as_deref()
                .map(|guest_target| Self::Aggregate {
                    guest_target,
                    children: &options.microvm.microvm_mount_child,
                })
        }
    }
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

/// Resolves the host paths that one of `--mount-deny`, `--mount-allow`, or
/// `--mount-write` requests in the export root `root_path` to unique, canonical
/// relative paths in lexical order. Each must exist inside the export root, on
/// its filesystem, and be reachable without a symbolic link.
fn canonical_microvm_filesystem_policy_paths(
    root_path: &Path,
    requested: &[PathBuf],
    kind: openvmm_defs::microvm::MicrovmFilesystemPathKind,
) -> anyhow::Result<Vec<String>> {
    #[cfg(unix)]
    let root_metadata = fs_err::symlink_metadata(root_path).with_context(|| {
        format!(
            "failed to inspect microVM filesystem root {}",
            root_path.display()
        )
    })?;
    let mut relative_paths = Vec::with_capacity(requested.len());
    for path in requested {
        anyhow::ensure!(
            !path.as_os_str().is_empty()
                && !path.components().any(|component| matches!(
                    component,
                    std::path::Component::CurDir | std::path::Component::ParentDir
                )),
            "microVM {kind} path is empty or contains a dot or parent component: {}",
            path.display()
        );
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            root_path.join(path)
        };
        let canonical = fs_err::canonicalize(&absolute).with_context(|| {
            format!(
                "failed to canonicalize microVM {kind} path {}",
                absolute.display()
            )
        })?;
        // Canonicalizing resolves links, so inspect the requested path itself.
        let mut current = PathBuf::new();
        for component in absolute.components() {
            current.push(component.as_os_str());
            if !matches!(component, std::path::Component::Normal(_)) {
                continue;
            }
            let metadata = fs_err::symlink_metadata(&current).with_context(|| {
                format!(
                    "failed to inspect microVM {kind} path component {}",
                    current.display()
                )
            })?;
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "microVM {kind} path component is a symbolic link: {}",
                current.display()
            );
            #[cfg(windows)]
            anyhow::ensure!(
                std::os::windows::fs::MetadataExt::file_attributes(&metadata) & 0x400 == 0,
                "microVM {kind} path component is a reparse point: {}",
                current.display()
            );
        }
        let relative = canonical.strip_prefix(root_path).with_context(|| {
            format!(
                "microVM {kind} path resolves outside the filesystem export root: {}",
                path.display()
            )
        })?;
        // A denied root hides everything but the allowed paths inside it.
        anyhow::ensure!(
            !relative.as_os_str().is_empty()
                || kind == openvmm_defs::microvm::MicrovmFilesystemPathKind::Denied,
            "microVM {kind} path cannot be the complete filesystem export"
        );
        anyhow::ensure!(
            relative
                .components()
                .all(|component| matches!(component, std::path::Component::Normal(_))),
            "microVM {kind} path contains a non-normal component"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = fs_err::symlink_metadata(&canonical)?;
            anyhow::ensure!(
                metadata.dev() == root_metadata.dev(),
                "microVM {kind} path crosses a nested mount: {}",
                path.display()
            );
        }
        relative_paths.push(relative.to_owned());
    }
    let mut encoded = relative_paths
        .iter()
        .map(|path| {
            path.iter()
                .map(|component| {
                    component
                        .to_str()
                        .with_context(|| format!("microVM {kind} path is not valid UTF-8"))
                })
                .collect::<anyhow::Result<Vec<_>>>()
                .map(|components| components.join("/"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    // The snapshot contract records the paths in lexical order.
    encoded.sort_unstable();
    for pair in encoded.windows(2) {
        anyhow::ensure!(
            pair[0] != pair[1],
            "microVM {kind} paths must be unique{}",
            match kind {
                openvmm_defs::microvm::MicrovmFilesystemPathKind::Allowed => "",
                _ => " and non-overlapping",
            }
        );
    }
    openvmm_defs::microvm::validate_microvm_filesystem_policy_paths(kind, &encoded)?;
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

/// The host paths that `--mount-deny`, `--mount-allow`, and `--mount-write`
/// request, each in the `--mount` whose host directory contains it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MicrovmFilesystemPolicyPaths<'a> {
    /// Paths hidden from the guest.
    pub(crate) denied: &'a [PathBuf],
    /// Paths inside denied paths that the guest can reach again.
    pub(crate) allowed: &'a [PathBuf],
    /// The only paths of a read-write export that the guest can modify.
    pub(crate) writable: &'a [PathBuf],
}

/// Builds the filesystem of `requested` in its canonical host root, with the
/// access policy that `--mount-deny`, `--mount-allow`, and `--mount-write`
/// attributed to it.
fn microvm_filesystem_from_root(
    requested: &cli_args::microvm::MicrovmMountCli,
    (root_path, attachment): (
        String,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    ),
    policy: MicrovmFilesystemPolicyPaths<'_>,
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
) -> anyhow::Result<EffectiveMicrovmFilesystem> {
    use openvmm_defs::microvm::MicrovmFilesystemPathKind;

    if owner.is_caller() {
        validate_caller_owned_root(Path::new(&root_path))?;
    }
    let root = Path::new(&root_path);
    let config = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
        requested.guest_target.clone(),
        requested.access,
    )?
    .with_access_policy(
        canonical_microvm_filesystem_policy_paths(
            root,
            policy.denied,
            MicrovmFilesystemPathKind::Denied,
        )?,
        canonical_microvm_filesystem_policy_paths(
            root,
            policy.allowed,
            MicrovmFilesystemPathKind::Allowed,
        )?,
        canonical_microvm_filesystem_policy_paths(
            root,
            policy.writable,
            MicrovmFilesystemPathKind::Writable,
        )?,
    )?
    .with_owner(owner);
    Ok(EffectiveMicrovmFilesystem {
        config,
        root_path,
        attachment,
        children: Vec::new(),
    })
}

/// Builds the aggregate child `requested` in its canonical host root, with the
/// access policy that `--mount-deny`, `--mount-allow`, and `--mount-write`
/// attributed to it.
fn microvm_filesystem_child_from_root(
    requested: &cli_args::microvm::MicrovmMountChildCli,
    root_path: &str,
    policy: MicrovmFilesystemPolicyPaths<'_>,
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
) -> anyhow::Result<openvmm_defs::microvm::MicrovmFilesystemChildConfig> {
    use openvmm_defs::microvm::MicrovmFilesystemPathKind;

    if owner.is_caller() {
        validate_caller_owned_root(Path::new(root_path))?;
    }
    let root = Path::new(root_path);
    openvmm_defs::microvm::MicrovmFilesystemChildConfig::new(
        requested.name.clone(),
        requested.access,
    )?
    .with_access_policy(
        canonical_microvm_filesystem_policy_paths(
            root,
            policy.denied,
            MicrovmFilesystemPathKind::Denied,
        )?,
        canonical_microvm_filesystem_policy_paths(
            root,
            policy.allowed,
            MicrovmFilesystemPathKind::Allowed,
        )?,
        canonical_microvm_filesystem_policy_paths(
            root,
            policy.writable,
            MicrovmFilesystemPathKind::Writable,
        )?,
    )
    .with_context(|| format!("invalid policy of --mount-child {}", requested.name))
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

/// Attributes the host paths of one access-policy option to the export roots
/// that contain them: every path to the root of a single directory, which
/// resolves a relative path, or each absolute path to the aggregate child
/// whose root contains it.
fn attribute_microvm_filesystem_policy_paths(
    roots: &[(
        String,
        openvmm_helpers::snapshot::microvm::SnapshotAttachment,
    )],
    aggregate: bool,
    requested: &[PathBuf],
    option: &str,
    kind: openvmm_defs::microvm::MicrovmFilesystemPathKind,
) -> anyhow::Result<Vec<Vec<PathBuf>>> {
    let mut attributed = vec![Vec::new(); roots.len()];
    if !aggregate {
        anyhow::ensure!(
            roots.len() == 1 || requested.is_empty(),
            "{option} requires --mount or --mount-aggregate"
        );
        if let Some(paths) = attributed.first_mut() {
            paths.extend_from_slice(requested);
        }
        return Ok(attributed);
    }
    for path in requested {
        anyhow::ensure!(
            path.is_absolute(),
            "with --mount-aggregate, {option} requires an absolute host path: {}",
            path.display()
        );
        let canonical = fs_err::canonicalize(path).with_context(|| {
            format!(
                "failed to canonicalize microVM {kind} path {}",
                path.display()
            )
        })?;
        let index = roots
            .iter()
            .position(|(root, _)| canonical.starts_with(root))
            .with_context(|| {
                format!(
                    "microVM {kind} path resolves outside the filesystem export roots: {}",
                    path.display()
                )
            })?;
        attributed[index].push(path.clone());
    }
    Ok(attributed)
}

/// Builds the filesystem that `request` describes. Each `--mount-deny`,
/// `--mount-allow`, and `--mount-write` path applies to the export root that
/// contains it: the `--mount` root, against which a relative path resolves,
/// or the root of the `--mount-child` that contains the absolute path.
fn microvm_filesystem_from_request(
    request: MicrovmFilesystemRequest<'_>,
    policy: MicrovmFilesystemPolicyPaths<'_>,
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
) -> anyhow::Result<EffectiveMicrovmFilesystem> {
    use openvmm_defs::microvm::MicrovmFilesystemPathKind;

    let slot = &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0];
    let (aggregate, roots) = match request {
        MicrovmFilesystemRequest::Single(mount) => (
            false,
            vec![microvm_filesystem_attachment(&mount.host_path, slot)?],
        ),
        MicrovmFilesystemRequest::Aggregate { children, .. } => (
            true,
            children
                .iter()
                .map(|child| {
                    microvm_filesystem_attachment(&child.host_path, slot)
                        .with_context(|| format!("invalid --mount-child {}", child.name))
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        ),
    };
    validate_microvm_filesystem_roots(&roots)?;
    let attribute = |requested, option, kind| {
        attribute_microvm_filesystem_policy_paths(&roots, aggregate, requested, option, kind)
    };
    let denied = attribute(
        policy.denied,
        "--mount-deny",
        MicrovmFilesystemPathKind::Denied,
    )?;
    let allowed = attribute(
        policy.allowed,
        "--mount-allow",
        MicrovmFilesystemPathKind::Allowed,
    )?;
    let writable = attribute(
        policy.writable,
        "--mount-write",
        MicrovmFilesystemPathKind::Writable,
    )?;
    let policy = |index: usize| MicrovmFilesystemPolicyPaths {
        denied: &denied[index],
        allowed: &allowed[index],
        writable: &writable[index],
    };

    match request {
        MicrovmFilesystemRequest::Single(mount) => {
            let root = roots.into_iter().next().context("--mount has no root")?;
            microvm_filesystem_from_root(mount, root, policy(0), owner)
        }
        MicrovmFilesystemRequest::Aggregate {
            guest_target,
            children,
        } => {
            anyhow::ensure!(
                roots.iter().map(|(root, _)| root.len()).sum::<usize>() <= MAX_AGGREGATE_ROOT_BYTES,
                "the host directories of the --mount-child options exceed {MAX_AGGREGATE_ROOT_BYTES} bytes"
            );
            let child_configs = children
                .iter()
                .zip(&roots)
                .enumerate()
                .map(|(index, (child, (root, _)))| {
                    microvm_filesystem_child_from_root(child, root, policy(index), owner)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let config = openvmm_defs::microvm::MicrovmFilesystemConfig::new_aggregate(
                guest_target.to_owned(),
                child_configs,
            )?
            .with_owner(owner);
            let attachment =
                openvmm_helpers::snapshot::microvm::microvm_filesystem_aggregate_attachment(
                    slot.stable_id,
                    config
                        .children
                        .iter()
                        .zip(&roots)
                        .map(|(child, (_, attachment))| {
                            (
                                child.name.as_str(),
                                attachment.identity_kind.as_str(),
                                attachment.identity.as_slice(),
                            )
                        }),
                );
            Ok(EffectiveMicrovmFilesystem {
                config,
                root_path: String::new(),
                attachment,
                children: roots,
            })
        }
    }
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
    openvmm_helpers::snapshot::microvm::snapshot_microvm_filesystem_config(saved)
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

/// Returns the effective filesystem that `request` attaches. A restore must
/// supply exactly the snapshot's filesystem, unless the snapshot's slot is
/// dormant, in which case one `--mount` may bind a new filesystem to it.
pub(super) fn effective_microvm_filesystem(
    request: Option<MicrovmFilesystemRequest<'_>>,
    policy: MicrovmFilesystemPolicyPaths<'_>,
    owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
    restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
) -> anyhow::Result<Option<EffectiveMicrovmFilesystem>> {
    let Some(restore) = restore else {
        return request
            .map(|request| microvm_filesystem_from_request(request, policy, owner))
            .transpose();
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
    let Some(saved) = &restore.microvm_filesystem else {
        let Some(request) = request else {
            return Ok(None);
        };
        anyhow::ensure!(
            restore.microvm_filesystem_slot_version
                == openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION,
            "snapshot does not support restore-time microVM filesystem attachment"
        );
        // Only the dormant slot exists in the snapshot's machine.
        anyhow::ensure!(
            matches!(request, MicrovmFilesystemRequest::Single(_)),
            "a dormant-slot snapshot can attach only a --mount on restore"
        );
        return microvm_filesystem_from_request(request, policy, owner).map(Some);
    };
    let request =
        request.context("snapshot restore requires a fresh --mount attachment for fs:microvm0")?;
    let config = microvm_filesystem_from_snapshot(saved)?;
    match (request, config.is_aggregate()) {
        (MicrovmFilesystemRequest::Single(mount), false) => anyhow::ensure!(
            mount.guest_target == config.guest_mount_target && mount.access == config.access,
            "restore-time mount target or access mode does not match the snapshot contract"
        ),
        (
            MicrovmFilesystemRequest::Aggregate {
                guest_target,
                children,
            },
            true,
        ) => anyhow::ensure!(
            guest_target == config.guest_mount_target
                && children.len() == config.children.len()
                && children
                    .iter()
                    .zip(&config.children)
                    .all(|(requested, saved)| requested.name == saved.name
                        && requested.access == saved.access),
            "restore-time aggregate target, or --mount-child names, order, or access modes, do not match the snapshot contract"
        ),
        (_, false) => {
            anyhow::bail!("snapshot restore requires the --mount that the snapshot attached")
        }
        (_, true) => anyhow::bail!(
            "snapshot restore requires the --mount-aggregate and --mount-child options that the snapshot attached"
        ),
    }
    anyhow::ensure!(
        owner == config.owner,
        "restore-time --mount-owner {} does not match the snapshot contract ({})",
        owner.as_str(),
        config.owner.as_str()
    );
    let effective = microvm_filesystem_from_request(request, policy, owner)?;
    if config.is_aggregate() {
        anyhow::ensure!(
            effective
                .children
                .iter()
                .zip(&saved.children)
                .all(|((root_path, _), saved)| *root_path == saved.canonical_host_path),
            "restore-time --mount-child canonical host paths do not match the snapshot contract"
        );
    } else {
        anyhow::ensure!(
            !saved.canonical_host_path.is_empty(),
            "snapshot filesystem canonical host path is missing; this snapshot predates path-bound filesystem restore"
        );
        anyhow::ensure!(
            effective.root_path == saved.canonical_host_path,
            "restore-time filesystem canonical host path does not match the snapshot contract"
        );
    }
    anyhow::ensure!(
        restore.attachments.contains(&effective.attachment),
        "restore-time filesystem root identity does not match the snapshot attachment"
    );
    anyhow::ensure!(
        effective.config == config,
        "restore-time filesystem access policy (denied, allowed, or writable paths) does not match the snapshot contract"
    );
    Ok(Some(effective))
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
    const DENIED: openvmm_defs::microvm::MicrovmFilesystemPathKind =
        openvmm_defs::microvm::MicrovmFilesystemPathKind::Denied;

    /// Resolves the `--mount` of a single directory.
    fn single_filesystem(
        requested: Option<&cli_args::microvm::MicrovmMountCli>,
        denied_paths: &[PathBuf],
        owner: openvmm_defs::microvm::MicrovmFilesystemOwner,
        restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
    ) -> anyhow::Result<Option<EffectiveMicrovmFilesystem>> {
        let policy = MicrovmFilesystemPolicyPaths {
            denied: denied_paths,
            ..Default::default()
        };
        effective_microvm_filesystem(
            requested.map(MicrovmFilesystemRequest::Single),
            policy,
            owner,
            restore,
        )
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
        share_contract(Some((&filesystem, root)))
    }

    fn dormant_filesystem_contract() -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract
    {
        share_contract(None)
    }

    /// Builds the contract of a cold-booted machine with `share` attached to
    /// the virtio-fs slot, or with a dormant slot.
    fn share_contract(
        share: Option<(&openvmm_defs::microvm::MicrovmFilesystemConfig, &Path)>,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let slot = &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0];
        let root = share.map(|(_, root)| microvm_filesystem_attachment(root, slot).unwrap());
        let effective = share
            .zip(root)
            .map(
                |((config, _), (root_path, attachment))| EffectiveMicrovmFilesystem {
                    config: config.clone(),
                    root_path,
                    attachment,
                    children: Vec::new(),
                },
            );
        effective_contract(effective.as_ref())
    }

    /// Builds the contract of a cold-booted machine with `filesystem`
    /// attached to the virtio-fs slot, or with a dormant slot.
    fn effective_contract(
        filesystem: Option<&EffectiveMicrovmFilesystem>,
    ) -> openvmm_helpers::snapshot::microvm::SnapshotMachineContract {
        let slot = &openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS[0];
        let mut command_line = build_microvm_command_line(&[], false).unwrap();
        openvmm_defs::microvm::append_microvm_virtio_discovery(
            &mut command_line,
            None,
            true,
            &filesystem
                .iter()
                .map(|filesystem| filesystem.config.clone())
                .collect::<Vec<_>>(),
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
        state_unit_names.push(format!("virtiofs-{}", slot.mmio_base));
        openvmm_helpers::snapshot::microvm::microvm_machine_contract(
            if cfg!(windows) { "whp" } else { "kvm" },
            openvmm_helpers::snapshot::microvm::MICROVM_BOOT_LAYOUT_VERSION,
            command_line,
            None,
            true,
            filesystem.map(|filesystem| {
                openvmm_helpers::snapshot::microvm::MicrovmFilesystemSource {
                    config: &filesystem.config,
                    canonical_host_path: Path::new(&filesystem.root_path),
                    attachment: filesystem.attachment.clone(),
                    children: filesystem
                        .children
                        .iter()
                        .map(|(root_path, attachment)| (Path::new(root_path), attachment))
                        .collect(),
                }
            }),
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
        let filesystem = single_filesystem(
            options.microvm.microvm_mount.as_ref(),
            &options.microvm.microvm_mount_deny,
            VMM,
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(filesystem.config.denied_paths, vec!["secrets".to_owned()]);

        let outside = tempfile::tempdir().unwrap();
        assert!(
            canonical_microvm_filesystem_policy_paths(
                root.path(),
                &[outside.path().to_owned()],
                DENIED
            )
            .is_err()
        );
        assert!(
            canonical_microvm_filesystem_policy_paths(
                root.path(),
                &[secrets.clone(), secrets],
                DENIED
            )
            .is_err()
        );
    }

    #[test]
    fn filesystem_restore_requires_same_live_root_identity() {
        let root = tempfile::tempdir().unwrap();
        let contract = filesystem_contract(root.path());
        let options = restore_mount_options(root.path(), "ro");
        let restored = single_filesystem(
            options.microvm.microvm_mount.as_ref(),
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
            single_filesystem(
                replacement_options.microvm.microvm_mount.as_ref(),
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
            single_filesystem(
                options.microvm.microvm_mount.as_ref(),
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
        assert!(single_filesystem(None, &[], VMM, Some(&contract)).is_err());

        let changed_mode = restore_mount_options(root.path(), "rw");
        assert!(
            single_filesystem(
                changed_mode.microvm.microvm_mount.as_ref(),
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
            single_filesystem(None, &[], VMM, Some(&contract))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn filesystem_restore_attaches_mount_to_dormant_slot() {
        let root = tempfile::tempdir().unwrap();
        let contract = dormant_filesystem_contract();
        let options = restore_mount_options(root.path(), "rw");
        let filesystem = single_filesystem(
            options.microvm.microvm_mount.as_ref(),
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
        let error = match single_filesystem(
            options.microvm.microvm_mount.as_ref(),
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
        assert!(
            parse(&["--mount-owner", "caller"])
                .unwrap()
                .validate_microvm_options()
                .is_err()
        );
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
            let error = match single_filesystem(
                options.microvm.microvm_mount.as_ref(),
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
            let filesystem =
                single_filesystem(options.microvm.microvm_mount.as_ref(), &[], caller, None)
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
        let requested = options.microvm.microvm_mount.as_ref();

        let contract = filesystem_contract_with_owner(root.path(), caller);
        assert_eq!(
            contract.microvm_filesystem.as_ref().unwrap().owner_mode,
            "caller"
        );
        let restored = single_filesystem(requested, &[], caller, Some(&contract))
            .unwrap()
            .unwrap();
        assert_eq!(restored.config.owner, caller);
        let error = match single_filesystem(requested, &[], VMM, Some(&contract)) {
            Err(error) => error,
            Ok(_) => panic!("restore accepted a different ownership mode"),
        };
        assert!(error.to_string().contains("--mount-owner vmm"));

        // Snapshots record VMM ownership as an empty mode, like those that
        // predate the field.
        let contract = filesystem_contract(root.path());
        assert_eq!(contract.microvm_filesystem.as_ref().unwrap().owner_mode, "");
        assert!(single_filesystem(requested, &[], caller, Some(&contract)).is_err());
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

    fn aggregate_options(children: &[String], extra: &[&str]) -> Options {
        let mut args = vec![
            "openvmm",
            "--machine",
            "microvm",
            "--mount-aggregate",
            "/run/nvx/shares",
        ];
        for child in children {
            args.extend(["--mount-child", child.as_str()]);
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
            format!("work,{},rw", workspace.display()),
            format!("tools,{},{toolcache_mode}", toolcache.display()),
        ]
    }

    /// Resolves the filesystem and access policy that `options` request.
    fn options_filesystem(
        options: &Options,
        restore: Option<&openvmm_helpers::snapshot::microvm::SnapshotMachineContract>,
    ) -> anyhow::Result<EffectiveMicrovmFilesystem> {
        effective_microvm_filesystem(
            MicrovmFilesystemRequest::from_options(options),
            MicrovmFilesystemPolicyPaths {
                denied: &options.microvm.microvm_mount_deny,
                allowed: &options.microvm.microvm_mount_allow,
                writable: &options.microvm.microvm_mount_write,
            },
            VMM,
            restore,
        )?
        .context("the options request no filesystem")
    }

    #[test]
    fn mount_is_not_repeatable_and_conflicts_with_an_aggregate() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let first_mount = format!("/a,{}", first.path().display());
        let second_mount = format!("/b,{}", second.path().display());
        let child = format!("0,{}", second.path().display());
        for arguments in [
            vec!["--mount", &first_mount, "--mount", &second_mount],
            vec![
                "--mount",
                &first_mount,
                "--mount-aggregate",
                "/b",
                "--mount-child",
                &child,
            ],
            vec!["--mount-child", &child],
        ] {
            assert!(
                Options::try_parse_from(
                    ["openvmm", "--machine", "microvm"]
                        .into_iter()
                        .chain(arguments.iter().copied())
                )
                .is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn mount_child_names_and_modes_parse() {
        use cli_args::microvm::MicrovmMountChildCli;
        use openvmm_defs::microvm::MicrovmFilesystemAccess;
        use std::str::FromStr as _;

        let child = MicrovmMountChildCli::from_str("work,/srv/a,b,rw").unwrap();
        assert_eq!(child.name, "work");
        assert_eq!(child.host_path, PathBuf::from("/srv/a,b"));
        assert_eq!(child.access, MicrovmFilesystemAccess::ReadWrite);
        let child = MicrovmMountChildCli::from_str("0,/srv/a,b").unwrap();
        assert_eq!(child.host_path, PathBuf::from("/srv/a,b"));
        assert_eq!(child.access, MicrovmFilesystemAccess::ReadOnly);
        let child = MicrovmMountChildCli::from_str("0,/srv/a,ro").unwrap();
        assert_eq!(child.host_path, PathBuf::from("/srv/a"));
        for invalid in ["", "0", "0,", "0,,rw", "a b,/srv", ".,/srv", "a/b,/srv"] {
            assert!(
                MicrovmMountChildCli::from_str(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn aggregate_children_share_the_fixed_slot() {
        use openvmm_defs::microvm::MicrovmFilesystemAccess;

        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let options = aggregate_options(
            &workspace_and_toolcache(workspace.path(), toolcache.path(), "ro"),
            &[],
        );
        options.validate_microvm_options().unwrap();
        let filesystem = options_filesystem(&options, None).unwrap();
        assert!(filesystem.config.is_aggregate());
        assert_eq!(filesystem.config.guest_mount_target, "/run/nvx/shares");
        assert_eq!(filesystem.config.access, MicrovmFilesystemAccess::ReadWrite);
        assert_eq!(
            filesystem
                .config
                .children
                .iter()
                .map(|child| (child.name.as_str(), child.access))
                .collect::<Vec<_>>(),
            [
                ("work", MicrovmFilesystemAccess::ReadWrite),
                ("tools", MicrovmFilesystemAccess::ReadOnly),
            ]
        );
        assert!(filesystem.root_path.is_empty());
        assert_eq!(filesystem.attachment.stable_id, "fs:microvm0");
        assert_eq!(
            filesystem.attachment.identity_kind,
            openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_AGGREGATE_IDENTITY_KIND
        );
        for ((root_path, _), root) in filesystem.children.iter().zip([&workspace, &toolcache]) {
            assert_eq!(
                root_path,
                fs_err::canonicalize(root.path()).unwrap().to_str().unwrap()
            );
        }
        assert_ne!(
            filesystem.children[0].1.identity,
            filesystem.children[1].1.identity
        );

        // The aggregate's identity covers the root of every child.
        let other = tempfile::tempdir().unwrap();
        let changed = options_filesystem(
            &aggregate_options(
                &workspace_and_toolcache(workspace.path(), other.path(), "ro"),
                &[],
            ),
            None,
        )
        .unwrap();
        assert_ne!(changed.attachment, filesystem.attachment);
    }

    #[test]
    fn aggregate_requires_unique_valid_children() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let options = aggregate_options(&[], &[]);
        assert!(options.validate_microvm_options().is_err());
        let options = aggregate_options(
            &[
                format!("same,{}", first.path().display()),
                format!("same,{}", second.path().display()),
            ],
            &[],
        );
        let error = options.validate_microvm_options().unwrap_err();
        assert!(format!("{error:#}").contains("not unique"), "{error:#}");
        let options = Options::try_parse_from([
            "openvmm",
            "--machine",
            "microvm",
            "--mount-aggregate",
            "relative",
            "--mount-child",
            &format!("0,{}", first.path().display()),
        ])
        .unwrap();
        assert!(options.validate_microvm_options().is_err());
    }

    #[test]
    fn aggregate_policy_paths_apply_to_the_child_that_contains_them() {
        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let workspace_secrets = workspace.path().join("secrets");
        let toolcache_secrets = toolcache.path().join("credentials");
        fs_err::create_dir(&workspace_secrets).unwrap();
        fs_err::create_dir(&toolcache_secrets).unwrap();
        let children = workspace_and_toolcache(workspace.path(), toolcache.path(), "ro");
        let deny = |paths: &[&Path]| {
            let mut arguments = Vec::new();
            for path in paths {
                arguments.push("--mount-deny".to_owned());
                arguments.push(path.display().to_string());
            }
            let arguments = arguments.iter().map(String::as_str).collect::<Vec<_>>();
            options_filesystem(&aggregate_options(&children, &arguments), None)
        };
        let filesystem = deny(&[&toolcache_secrets, &workspace_secrets]).unwrap();
        assert_eq!(filesystem.config.children[0].denied_paths, ["secrets"]);
        assert_eq!(filesystem.config.children[1].denied_paths, ["credentials"]);

        // A relative path names no child.
        let error = deny(&[Path::new("secrets")]).unwrap_err();
        assert!(
            error.to_string().contains("absolute host path"),
            "{error:#}"
        );
        let outside = tempfile::tempdir().unwrap();
        let error = deny(&[outside.path()]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("outside the filesystem export roots"),
            "{error:#}"
        );
    }

    #[test]
    fn aggregate_children_reject_overlapping_host_directories() {
        let parent = tempfile::tempdir().unwrap();
        let nested = parent.path().join("nested");
        fs_err::create_dir(&nested).unwrap();
        for (first, second) in [
            (parent.path(), nested.as_path()),
            (nested.as_path(), parent.path()),
            (parent.path(), parent.path()),
        ] {
            let options = aggregate_options(&workspace_and_toolcache(first, second, "ro"), &[]);
            let error = match options_filesystem(&options, None) {
                Err(error) => error,
                Ok(_) => panic!("overlapping host directories were accepted"),
            };
            assert!(error.to_string().contains("must not overlap"), "{error:#}");
        }
    }

    #[test]
    fn restore_attaches_an_aggregate_only_where_the_snapshot_did() {
        let workspace = tempfile::tempdir().unwrap();
        let toolcache = tempfile::tempdir().unwrap();
        let children = workspace_and_toolcache(workspace.path(), toolcache.path(), "ro");
        let restore = ["--restore-snapshot", "snapshot"];

        // A dormant slot binds only a single directory on restore.
        let error = options_filesystem(
            &aggregate_options(&children, &restore),
            Some(&dormant_filesystem_contract()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("only a --mount"), "{error:#}");

        // A snapshot of a single directory requires that directory.
        let error = options_filesystem(
            &aggregate_options(&children, &restore),
            Some(&filesystem_contract(workspace.path())),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("requires the --mount"),
            "{error:#}"
        );
    }

    #[test]
    fn aggregate_restore_requires_the_same_children() {
        let workspace = policy_share();
        let toolcache = tempfile::tempdir().unwrap();
        let children = workspace_and_toolcache(workspace.path(), toolcache.path(), "ro");
        let deny = workspace.path().join("logs").display().to_string();
        let policy = ["--mount-deny", deny.as_str()];
        let captured = options_filesystem(&aggregate_options(&children, &policy), None).unwrap();
        let contract = effective_contract(Some(&captured));
        assert_eq!(
            contract.microvm_filesystem.as_ref().unwrap().children.len(),
            2
        );
        let restore = |children: &[String], policy: &[&str]| {
            let mut arguments = policy.to_vec();
            arguments.extend(["--restore-snapshot", "snapshot"]);
            options_filesystem(&aggregate_options(children, &arguments), Some(&contract))
        };
        let restored = restore(&children, &policy).unwrap();
        assert_eq!(restored.attachment, captured.attachment);
        assert_eq!(restored.config, captured.config);

        let other = tempfile::tempdir().unwrap();
        let swapped = vec![children[1].clone(), children[0].clone()];
        let renamed = vec![
            children[0].replace("work,", "workspace,"),
            children[1].clone(),
        ];
        let writable = workspace_and_toolcache(workspace.path(), toolcache.path(), "rw");
        let replaced = workspace_and_toolcache(workspace.path(), other.path(), "ro");
        for (changed, policy, expected) in [
            (&swapped, &policy[..], "do not match the snapshot contract"),
            (&renamed, &policy[..], "do not match the snapshot contract"),
            (&writable, &policy[..], "do not match the snapshot contract"),
            (
                &children[..1].to_vec(),
                &policy[..],
                "do not match the snapshot contract",
            ),
            (&replaced, &policy[..], "canonical host paths"),
            (&children, &[][..], "access policy"),
        ] {
            let error = restore(changed, policy).unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        let error = options_filesystem(
            &mount_options(
                &[format!("/run/nvx/shares,{},rw", workspace.path().display())],
                &["--restore-snapshot", "snapshot"],
            ),
            Some(&contract),
        )
        .unwrap_err();
        assert!(error.to_string().contains("--mount-aggregate"), "{error:#}");
    }

    /// Creates a share with a denied `logs` directory that holds an allowed
    /// `logs/payloads` directory, a writable `out` directory, and a writable
    /// `build.log` file.
    fn policy_share() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs_err::create_dir_all(root.path().join("logs").join("payloads")).unwrap();
        fs_err::create_dir(root.path().join("out")).unwrap();
        fs_err::write(root.path().join("build.log"), b"log").unwrap();
        root
    }

    fn policy_options(root: &Path, mode: &str, policy: &[&str]) -> Options {
        mount_options(&[format!("/workspace,{},{mode}", root.display())], policy)
    }

    #[test]
    fn mount_allow_and_mount_write_build_the_access_policy() {
        let root = policy_share();
        let options = policy_options(
            root.path(),
            "rw",
            &[
                "--mount-deny",
                "logs",
                "--mount-allow",
                &root
                    .path()
                    .join("logs")
                    .join("payloads")
                    .display()
                    .to_string(),
                "--mount-write",
                "out",
                "--mount-write",
                "build.log",
            ],
        );
        let filesystem = options_filesystem(&options, None).unwrap();
        options.validate_microvm_options().unwrap();
        let config = &filesystem.config;
        assert_eq!(config.denied_paths, ["logs"]);
        assert_eq!(config.allowed_paths, ["logs/payloads"]);
        assert_eq!(config.writable_paths, ["build.log", "out"]);

        for (mode, policy, expected) in [
            (
                "ro",
                &["--mount-write", "out"][..],
                "read-only microVM filesystem cannot have writable paths",
            ),
            (
                "rw",
                &["--mount-deny", "logs", "--mount-allow", "out"][..],
                "must be inside a denied path",
            ),
            (
                "rw",
                &["--mount-deny", "logs", "--mount-write", "logs/payloads"][..],
                "hidden by a denied path",
            ),
            (
                "rw",
                &["--mount-write", "out", "--mount-write", "out"][..],
                "writable paths must be unique",
            ),
            ("rw", &["--mount-write", "."][..], "dot or parent component"),
            (
                "rw",
                &["--mount-write", "missing"][..],
                "failed to canonicalize microVM writable path",
            ),
        ] {
            let options = policy_options(root.path(), mode, policy);
            let error = match options_filesystem(&options, None) {
                Err(error) => format!("{error:#}"),
                Ok(_) => panic!("unsafe access policy {policy:?} was accepted"),
            };
            assert!(error.contains(expected), "{error}");
        }
        let root_path = root.path().display().to_string();
        let options = policy_options(root.path(), "rw", &["--mount-write", &root_path]);
        let error = format!("{:#}", options_filesystem(&options, None).unwrap_err());
        assert!(
            error.contains("cannot be the complete filesystem export"),
            "{error}"
        );
    }

    #[test]
    fn mount_deny_hides_a_root_that_has_allowed_paths() {
        let root = policy_share();
        let root_path = root.path().display().to_string();
        let log = root.path().join("build.log").display().to_string();
        let options = policy_options(root.path(), "rw", &["--mount-deny", &root_path]);
        let error = format!("{:#}", options_filesystem(&options, None).unwrap_err());
        assert!(error.contains("hide its root only"), "{error}");
        let options = policy_options(
            root.path(),
            "rw",
            &["--mount-deny", &root_path, "--mount-allow", &log],
        );
        let config = options_filesystem(&options, None).unwrap().config;
        assert_eq!(config.denied_paths, [""]);
        assert_eq!(config.allowed_paths, ["build.log"]);

        // In an aggregate, the root is the child's.
        let other = tempfile::tempdir().unwrap();
        let children = workspace_and_toolcache(other.path(), root.path(), "ro");
        let options = aggregate_options(
            &children,
            &["--mount-deny", &root_path, "--mount-allow", &log],
        );
        let filesystem = options_filesystem(&options, None).unwrap();
        assert!(filesystem.config.children[0].denied_paths.is_empty());
        assert_eq!(filesystem.config.children[1].denied_paths, [""]);
        assert_eq!(filesystem.config.children[1].allowed_paths, ["build.log"]);
    }

    #[cfg(unix)]
    #[test]
    fn policy_paths_reject_symbolic_link_components() {
        use openvmm_defs::microvm::MicrovmFilesystemPathKind;

        let root = policy_share();
        std::os::unix::fs::symlink("logs", root.path().join("link")).unwrap();
        // Each path resolves inside the export, but only through the link.
        for kind in [
            MicrovmFilesystemPathKind::Denied,
            MicrovmFilesystemPathKind::Allowed,
            MicrovmFilesystemPathKind::Writable,
        ] {
            for requested in [
                PathBuf::from("link"),
                PathBuf::from("link/payloads"),
                root.path().join("link").join("payloads"),
            ] {
                let error = canonical_microvm_filesystem_policy_paths(
                    root.path(),
                    std::slice::from_ref(&requested),
                    kind,
                )
                .unwrap_err();
                assert!(
                    format!("{error:#}").contains("component is a symbolic link"),
                    "{requested:?}: {error:#}"
                );
            }
        }
        assert_eq!(
            canonical_microvm_filesystem_policy_paths(
                root.path(),
                &[PathBuf::from("logs/payloads")],
                MicrovmFilesystemPathKind::Writable,
            )
            .unwrap(),
            ["logs/payloads"]
        );
    }

    #[test]
    fn mount_write_applies_to_the_child_that_contains_it() {
        let workspace = policy_share();
        let toolcache = tempfile::tempdir().unwrap();
        let children = workspace_and_toolcache(workspace.path(), toolcache.path(), "ro");
        let out = workspace.path().join("out").display().to_string();
        let options = aggregate_options(&children, &["--mount-write", &out]);
        let filesystem = options_filesystem(&options, None).unwrap();
        assert_eq!(filesystem.config.children[0].writable_paths, ["out"]);
        assert!(filesystem.config.children[1].writable_paths.is_empty());

        let options = aggregate_options(&children, &["--mount-write", "out"]);
        let error = options_filesystem(&options, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--mount-write requires an absolute host path"),
            "{error:#}"
        );
        // A read-only child cannot have writable paths.
        let tools_out = toolcache.path().join("out");
        fs_err::create_dir(&tools_out).unwrap();
        let tools_out = tools_out.display().to_string();
        let options = aggregate_options(&children, &["--mount-write", &tools_out]);
        let error = options_filesystem(&options, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("cannot have writable paths"),
            "{error:#}"
        );
    }

    #[test]
    fn filesystem_restore_requires_the_same_access_policy() {
        let root = policy_share();
        let policy = [
            "--mount-deny",
            "logs",
            "--mount-allow",
            "logs/payloads",
            "--mount-write",
            "out",
        ];
        let saved = options_filesystem(&policy_options(root.path(), "rw", &policy), None)
            .unwrap()
            .config;
        let contract = share_contract(Some((&saved, root.path())));
        let restore = |policy: &[&str]| {
            let mut arguments = policy.to_vec();
            arguments.extend(["--restore-snapshot", "snapshot"]);
            options_filesystem(
                &policy_options(root.path(), "rw", &arguments),
                Some(&contract),
            )
        };
        restore(&policy).unwrap();
        for changed in [
            &policy[..4],
            &["--mount-deny", "logs", "--mount-write", "out"][..],
            &[
                "--mount-deny",
                "logs",
                "--mount-allow",
                "logs/payloads",
                "--mount-write",
                "build.log",
            ][..],
        ] {
            let error = restore(changed).unwrap_err();
            assert!(error.to_string().contains("access policy"), "{error:#}");
        }
    }
}
