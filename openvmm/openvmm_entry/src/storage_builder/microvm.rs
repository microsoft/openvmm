// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-role microVM sandbox block devices.

use super::StorageBuilder;
use super::VirtioBlkDisk;
use crate::cli_args::DiskCliKind;
use crate::disk_open;
use anyhow::Context;
use mesh::CellUpdater;
use openvmm_defs::config::Config;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmSandboxBlockConfig;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use std::path::Path;
use std::time::Duration;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

/// The fixed role of a microVM sandbox block device.
pub(super) struct SandboxBlock {
    role: MicrovmSandboxBlockRole,
    snapshot_source: Option<MicrovmSandboxBlockSource>,
}

/// The geometry of a microVM sandbox block: its length and block sizes, in
/// bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MicrovmSandboxBlockGeometry {
    pub(crate) length: u64,
    pub(crate) logical_block_size: u32,
    pub(crate) physical_block_size: u32,
}

impl MicrovmSandboxBlockGeometry {
    /// Returns the geometry that a snapshot recorded for `block`.
    pub(crate) fn of(
        block: &openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock,
    ) -> Self {
        Self {
            length: block.length,
            logical_block_size: block.logical_block_size,
            physical_block_size: block.physical_block_size,
        }
    }
}

/// Checks that an image of geometry `image` fits `capacity`, the captured
/// geometry of a platform snapshot's `role` block: it must have the same block
/// sizes and must not be longer.
fn check_platform_capacity(
    role: MicrovmSandboxBlockRole,
    image: MicrovmSandboxBlockGeometry,
    capacity: MicrovmSandboxBlockGeometry,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        image.logical_block_size == capacity.logical_block_size
            && image.physical_block_size == capacity.physical_block_size,
        "microVM {} image has logical and physical block sizes {} and {}, but the platform snapshot captured {} and {}",
        role.as_str(),
        image.logical_block_size,
        image.physical_block_size,
        capacity.logical_block_size,
        capacity.physical_block_size,
    );
    anyhow::ensure!(
        image.length <= capacity.length,
        "microVM {} image is {} bytes, longer than the platform snapshot's {}-byte capacity",
        role.as_str(),
        image.length,
        capacity.length,
    );
    Ok(())
}

pub(crate) struct MicrovmSandboxBlockSource {
    pub(crate) role: MicrovmSandboxBlockRole,
    pub(crate) read_only: bool,
    pub(crate) length: u64,
    pub(crate) logical_block_size: u32,
    pub(crate) physical_block_size: u32,
    /// The block's file, or `None` for the absent fresh scratch of a platform
    /// restore.
    pub(crate) file: Option<std::fs::File>,
}

impl MicrovmSandboxBlockSource {
    /// Returns the block's file, which every block but an absent scratch has.
    fn file(&self) -> anyhow::Result<&std::fs::File> {
        self.file
            .as_ref()
            .with_context(|| format!("microVM {} block has no file", self.role.as_str()))
    }
}

/// Returns a disk resource of `geometry` that reads `disk` for the sectors
/// that it has, and zeros past its end or everywhere without one, and that
/// refuses writes.
fn zero_fill_disk(
    disk: Option<Resource<DiskHandleKind>>,
    geometry: MicrovmSandboxBlockGeometry,
) -> Resource<DiskHandleKind> {
    Resource::new(disk_backend_resources::ZeroFillDiskHandle {
        disk,
        sector_count: geometry.length / u64::from(geometry.logical_block_size),
        sector_size: geometry.logical_block_size,
        physical_sector_size: geometry.physical_block_size,
    })
}

/// Returns the block contract that a capture records for `sources`.
///
/// A capture whose tier binds the blocks passes their storage `generation`,
/// which it records as the identity of each read-only layer and of a paired
/// scratch. A platform capture passes none, and records its layers as
/// unbound. A fresh scratch has no identity. The capture never reads a block
/// to identify it: it only flushes a paired scratch, which publication copies.
pub(crate) fn capture_snapshot_block_contract(
    sources: &[MicrovmSandboxBlockSource],
    scratch_policy: chipset_resources::microvm::MicrovmSnapshotScratchPolicy,
    generation: Option<[u8; 16]>,
    scratch_restore_mode: &str,
) -> anyhow::Result<Vec<openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock>> {
    sources
        .iter()
        .map(|source| {
            if !source.read_only
                && scratch_policy
                    == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
            {
                source.file()?.sync_all().with_context(|| {
                    format!("failed to flush microVM {} block", source.role.as_str())
                })?;
            }
            let (identity_kind, identity, artifact) = if source.role
                == MicrovmSandboxBlockRole::Scratch
                && scratch_policy == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
            {
                ("fresh", Vec::new(), String::new())
            } else if let Some(generation) = generation {
                let artifact = if source.role == MicrovmSandboxBlockRole::Scratch {
                    openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME.to_owned()
                } else {
                    String::new()
                };
                (
                    openvmm_helpers::snapshot::microvm::SNAPSHOT_BLOCK_IDENTITY_GENERATION,
                    generation.to_vec(),
                    artifact,
                )
            } else {
                anyhow::ensure!(
                    source.read_only,
                    "microVM paired scratch requires a storage generation"
                );
                ("unbound", Vec::new(), String::new())
            };
            Ok(
                openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock {
                    role: source.role.as_str().to_owned(),
                    read_only: source.read_only,
                    length: source.length,
                    identity_kind: identity_kind.to_owned(),
                    identity,
                    artifact,
                    logical_block_size: source.logical_block_size,
                    physical_block_size: source.physical_block_size,
                    restore_mode: if source.role == MicrovmSandboxBlockRole::Scratch
                        && scratch_policy
                            == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
                    {
                        scratch_restore_mode.to_owned()
                    } else {
                        String::new()
                    },
                },
            )
        })
        .collect()
}

/// Returns the block contract that a restore of `sources` must match, which
/// the restore compares with `saved_blocks`, the snapshot's.
///
/// The identity of each block that the snapshot binds is `generation`, the
/// storage generation of the restore's blocks, so that a snapshot that binds
/// other storage fails the comparison; a restore of such a snapshot requires
/// it. Unbound layers and fresh scratch have no identity. The restore never
/// reads a block to identify it.
pub(crate) fn restore_snapshot_block_contract(
    sources: &[MicrovmSandboxBlockSource],
    scratch_policy: chipset_resources::microvm::MicrovmSnapshotScratchPolicy,
    saved_blocks: &[openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock],
    generation: Option<[u8; 16]>,
) -> anyhow::Result<Vec<openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock>> {
    sources
        .iter()
        .map(|source| {
            let saved = saved_blocks
                .iter()
                .find(|block| block.role == source.role.as_str())
                .with_context(|| {
                    format!(
                        "snapshot is missing the microVM {} block contract",
                        source.role.as_str()
                    )
                })?;
            let identity = match saved.identity_kind.as_str() {
                openvmm_helpers::snapshot::microvm::SNAPSHOT_BLOCK_IDENTITY_GENERATION => generation
                    .with_context(|| {
                        format!(
                            "snapshot binds the microVM {} block to a storage generation, which the restore requires",
                            source.role.as_str()
                        )
                    })?
                    .to_vec(),
                "unbound" | "fresh" => Vec::new(),
                other => anyhow::bail!(
                    "snapshot microVM {} block identity kind '{other}' is unsupported",
                    source.role.as_str()
                ),
            };
            Ok(
                openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock {
                    role: source.role.as_str().to_owned(),
                    read_only: source.read_only,
                    length: source.length,
                    identity_kind: saved.identity_kind.clone(),
                    identity,
                    artifact: if source.role == MicrovmSandboxBlockRole::Scratch
                        && scratch_policy
                            == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
                    {
                        openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME.to_owned()
                    } else {
                        String::new()
                    },
                    logical_block_size: source.logical_block_size,
                    physical_block_size: source.physical_block_size,
                    restore_mode: saved.restore_mode.clone(),
                },
            )
        })
        .collect()
}

impl StorageBuilder {
    /// Adds a fixed-role microVM sandbox block device.
    ///
    /// `capacity`, the captured geometry of a platform snapshot's `distro`
    /// block at its restore, makes the device present that geometry over a
    /// file that may be shorter: reads past the file's end return zeros, and
    /// nothing resizes, pads, or writes the file. The file must have the
    /// captured block sizes and must not be longer. Only a snapshot-capable
    /// block has a capacity.
    pub async fn add_microvm_sandbox_block(
        &mut self,
        role: MicrovmSandboxBlockRole,
        kind: &DiskCliKind,
        read_only: bool,
        snapshot_capable: bool,
        capacity: Option<MicrovmSandboxBlockGeometry>,
    ) -> anyhow::Result<()> {
        assert!(
            snapshot_capable || capacity.is_none(),
            "only a snapshot-capable microVM sandbox block has a captured capacity"
        );
        if !snapshot_capable {
            self.vtl0_virtio_blk_disks.push(VirtioBlkDisk {
                disk: disk_open(kind, read_only).await?,
                read_only,
                serial: None,
                microvm: Some(SandboxBlock {
                    role,
                    snapshot_source: None,
                }),
            });
            return Ok(());
        }
        let (kind, delay_ms) = match kind {
            DiskCliKind::DelayDiskWrapper { delay_ms, disk } => (disk.as_ref(), Some(*delay_ms)),
            kind => (kind, None),
        };
        let DiskCliKind::File {
            path,
            create_with_len,
            direct,
        } = kind
        else {
            anyhow::bail!("snapshot-capable microVM sandbox blocks require a plain file backend");
        };
        anyhow::ensure!(
            !direct,
            "snapshot-capable microVM sandbox blocks do not support direct I/O"
        );
        anyhow::ensure!(
            !matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("vhd" | "vhdx" | "vmgs" | "iso")
            ),
            "snapshot-capable microVM sandbox blocks require an unformatted raw file backend"
        );

        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(!read_only || create_with_len.is_some());
        if create_with_len.is_some() {
            options.create(true).truncate(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
            use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
        }
        let file = options
            .open(path)
            .with_context(|| format!("failed to open sandbox block {}", path.display()))?;
        if let Some(length) = create_with_len {
            file.set_len(*length)
                .with_context(|| format!("failed to size sandbox block {}", path.display()))?;
        }
        self.add_snapshot_capable_block_file(role, file, path, read_only, capacity, delay_ms)
    }

    /// Adds the paired scratch of a direct-claimed restore: `file`, the
    /// snapshot's own `scratch.img` at `path`, which the restore opened for
    /// writing, so that the device writes the claimed scratch in place.
    pub fn add_microvm_claimed_scratch(
        &mut self,
        file: std::fs::File,
        path: &Path,
    ) -> anyhow::Result<()> {
        self.add_snapshot_capable_block_file(
            MicrovmSandboxBlockRole::Scratch,
            file,
            path,
            false,
            None,
            None,
        )
    }

    /// Adds a snapshot-capable sandbox block device of `role` over `file`, which
    /// was opened at `path`, as [`Self::add_microvm_sandbox_block`] describes,
    /// with a delay of `delay_ms` milliseconds on each request if set.
    fn add_snapshot_capable_block_file(
        &mut self,
        role: MicrovmSandboxBlockRole,
        file: std::fs::File,
        path: &Path,
        read_only: bool,
        capacity: Option<MicrovmSandboxBlockGeometry>,
        delay_ms: Option<u64>,
    ) -> anyhow::Result<()> {
        let metadata = file
            .metadata()
            .with_context(|| format!("failed to inspect sandbox block {}", path.display()))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "microVM sandbox block is not a regular file: {}",
            path.display()
        );
        let length = metadata.len();
        anyhow::ensure!(
            length != 0 && length % 512 == 0,
            "microVM sandbox block length must be nonzero and 512-byte aligned"
        );
        let logical_block_size: u32 = 512;
        #[cfg(target_os = "linux")]
        let physical_block_size = {
            use std::os::unix::fs::MetadataExt as _;
            u32::try_from(metadata.blksize())
                .context("sandbox block filesystem block size does not fit in u32")?
        };
        #[cfg(windows)]
        let physical_block_size: u32 = 4096;
        #[cfg(not(any(target_os = "linux", windows)))]
        let physical_block_size = logical_block_size;
        anyhow::ensure!(
            physical_block_size >= logical_block_size && physical_block_size.is_power_of_two(),
            "microVM sandbox block has unsupported physical block geometry"
        );
        let image = MicrovmSandboxBlockGeometry {
            length,
            logical_block_size,
            physical_block_size,
        };
        let presented = if let Some(capacity) = capacity {
            check_platform_capacity(role, image, capacity)?;
            capacity
        } else {
            image
        };
        let disk_file = file
            .try_clone()
            .context("failed to duplicate microVM sandbox block handle")?;
        #[cfg(target_os = "linux")]
        let disk = Resource::new(disk_backend_resources::BlockDeviceDiskHandle { file: disk_file });
        #[cfg(not(target_os = "linux"))]
        let disk = Resource::new(disk_backend_resources::FileDiskHandle(disk_file));
        let disk = if presented.length != image.length {
            zero_fill_disk(Some(disk), presented)
        } else {
            disk
        };
        let disk = if let Some(delay_ms) = delay_ms {
            Resource::new(disk_backend_resources::DelayDiskHandle {
                disk,
                delay: CellUpdater::new(Duration::from_millis(delay_ms)).cell(),
            })
        } else {
            disk
        };
        self.vtl0_virtio_blk_disks.push(VirtioBlkDisk {
            disk,
            read_only,
            serial: None,
            microvm: Some(SandboxBlock {
                role,
                snapshot_source: Some(MicrovmSandboxBlockSource {
                    role,
                    read_only,
                    length: presented.length,
                    logical_block_size: presented.logical_block_size,
                    physical_block_size: presented.physical_block_size,
                    file: Some(file),
                }),
            }),
        });
        Ok(())
    }

    /// Adds the fresh scratch of a platform restore with no file, at its
    /// captured `geometry`.
    ///
    /// The device stays writable, as the guest negotiated it at the template's
    /// boot and as the restore contract records it, but it has nowhere to keep
    /// data: reads return zeros, and writes fail with an I/O error.
    pub fn add_microvm_absent_scratch(&mut self, geometry: MicrovmSandboxBlockGeometry) {
        let role = MicrovmSandboxBlockRole::Scratch;
        self.vtl0_virtio_blk_disks.push(VirtioBlkDisk {
            disk: zero_fill_disk(None, geometry),
            read_only: role.is_read_only(),
            serial: None,
            microvm: Some(SandboxBlock {
                role,
                snapshot_source: Some(MicrovmSandboxBlockSource {
                    role,
                    read_only: role.is_read_only(),
                    length: geometry.length,
                    logical_block_size: geometry.logical_block_size,
                    physical_block_size: geometry.physical_block_size,
                    file: None,
                }),
            }),
        });
    }

    /// Adds the sandbox block devices of a microVM at their fixed virtio-mmio
    /// slots, in role order, and their snapshot sources to `resources`.
    pub(crate) fn build_microvm_sandbox_blocks(
        &mut self,
        config: &mut Config,
        resources: &mut crate::microvm::MicrovmResources,
    ) -> anyhow::Result<()> {
        if config.machine_profile != MachineProfile::Microvm {
            return Ok(());
        }
        let mut vtl0_virtio_blk_disks = std::mem::take(&mut self.vtl0_virtio_blk_disks);
        anyhow::ensure!(
            vtl0_virtio_blk_disks
                .iter()
                .all(|disk| disk.microvm.is_some()),
            "microVM requires roles for every virtio-blk device"
        );
        vtl0_virtio_blk_disks.sort_by_key(|disk| disk.microvm.as_ref().map(|block| block.role));
        for mut vblk in vtl0_virtio_blk_disks {
            if let Some(block) = vblk.microvm.take() {
                config
                    .microvm
                    .sandbox_blocks
                    .push(MicrovmSandboxBlockConfig {
                        role: block.role,
                        read_only: vblk.read_only,
                    });
                if let Some(source) = block.snapshot_source {
                    resources.sandbox_block_sources.push(source);
                }
            }
            config
                .virtio_devices
                .push((VirtioBus::Mmio, vblk.into_resource()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use test_with_tracing::test;

    const MIB: u64 = 1024 * 1024;

    fn geometry(
        length: u64,
        logical_block_size: u32,
        physical_block_size: u32,
    ) -> MicrovmSandboxBlockGeometry {
        MicrovmSandboxBlockGeometry {
            length,
            logical_block_size,
            physical_block_size,
        }
    }

    /// Attaches `path` as the read-only `distro` block of a snapshot-capable
    /// microVM, with `capacity`, and returns the attached block's disk
    /// resource kind and presented geometry.
    fn attach_distro(
        path: &Path,
        capacity: Option<MicrovmSandboxBlockGeometry>,
    ) -> anyhow::Result<(String, MicrovmSandboxBlockGeometry)> {
        let mut storage = StorageBuilder::new(None);
        futures::executor::block_on(storage.add_microvm_sandbox_block(
            MicrovmSandboxBlockRole::Distro,
            &DiskCliKind::File {
                path: path.to_owned(),
                create_with_len: None,
                direct: false,
            },
            true,
            true,
            capacity,
        ))?;
        let [block] = storage.vtl0_virtio_blk_disks.as_slice() else {
            panic!("one block is attached");
        };
        assert!(block.read_only);
        let source = block
            .microvm
            .as_ref()
            .and_then(|block| block.snapshot_source.as_ref())
            .expect("a snapshot-capable block has a source");
        Ok((
            block.disk.id().to_owned(),
            geometry(
                source.length,
                source.logical_block_size,
                source.physical_block_size,
            ),
        ))
    }

    #[test]
    fn platform_capacity_admits_no_longer_images_with_its_block_sizes() {
        let capacity = geometry(MIB, 512, 4096);
        for length in [512, 4096, MIB - 512, MIB] {
            check_platform_capacity(
                MicrovmSandboxBlockRole::Distro,
                geometry(length, 512, 4096),
                capacity,
            )
            .unwrap();
        }
        let err = check_platform_capacity(
            MicrovmSandboxBlockRole::Distro,
            geometry(MIB + 512, 512, 4096),
            capacity,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "microVM distro image is 1049088 bytes, longer than the platform snapshot's 1048576-byte capacity"
        );
        for image in [
            geometry(MIB, 512, 512),
            geometry(MIB, 512, 8192),
            geometry(MIB, 4096, 4096),
        ] {
            let err = check_platform_capacity(MicrovmSandboxBlockRole::Distro, image, capacity)
                .unwrap_err();
            assert!(
                err.to_string()
                    .starts_with("microVM distro image has logical and physical block sizes"),
                "{err:#}"
            );
        }
    }

    #[test]
    fn platform_distro_presents_its_capacity_over_a_shorter_image() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("distro.img");
        let contents = vec![0x3c; 64 * 1024];
        std::fs::write(&path, &contents).unwrap();
        let file_kind = if cfg!(target_os = "linux") {
            "block"
        } else {
            "file"
        };

        // Without a capacity, the device presents the image's own geometry.
        let (kind, image) = attach_distro(&path, None).unwrap();
        assert_eq!(kind, file_kind);
        assert_eq!(image.length, 64 * 1024);
        assert_eq!(image.logical_block_size, 512);

        // A capacity of the image's length attaches it as it is.
        assert_eq!(
            attach_distro(&path, Some(image)).unwrap(),
            (file_kind.to_owned(), image)
        );

        // A larger capacity is presented, and its tail reads as zeros.
        let capacity = MicrovmSandboxBlockGeometry {
            length: MIB,
            ..image
        };
        assert_eq!(
            attach_distro(&path, Some(capacity)).unwrap(),
            ("zerofill".to_owned(), capacity)
        );

        // A longer image, and one with other block sizes, are refused.
        let smaller = MicrovmSandboxBlockGeometry {
            length: 32 * 1024,
            ..image
        };
        let err = attach_distro(&path, Some(smaller)).unwrap_err();
        assert!(err.to_string().contains("longer than"), "{err:#}");
        let other_physical = MicrovmSandboxBlockGeometry {
            physical_block_size: image.physical_block_size * 2,
            ..capacity
        };
        let err = attach_distro(&path, Some(other_physical)).unwrap_err();
        assert!(err.to_string().contains("block sizes"), "{err:#}");

        // Nothing resized, padded, or wrote the image.
        assert_eq!(std::fs::read(&path).unwrap(), contents);
    }

    #[test]
    fn absent_scratch_is_writable_and_has_no_file() {
        let captured = geometry(MIB, 512, 4096);
        let mut storage = StorageBuilder::new(None);
        storage.add_microvm_absent_scratch(captured);
        let [block] = storage.vtl0_virtio_blk_disks.as_slice() else {
            panic!("one block is attached");
        };
        // The guest negotiated the scratch as writable, and a zero-fill disk
        // without a backing disk serves it.
        assert!(!block.read_only);
        assert_eq!(block.disk.id(), "zerofill");
        let sandbox = block.microvm.as_ref().unwrap();
        assert_eq!(sandbox.role, MicrovmSandboxBlockRole::Scratch);
        let source = sandbox.snapshot_source.as_ref().unwrap();
        assert!(source.file.is_none());

        // Its restore contract is the captured fresh scratch's.
        let saved = openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock {
            role: "scratch".to_owned(),
            read_only: false,
            length: MIB,
            identity_kind: "fresh".to_owned(),
            identity: Vec::new(),
            artifact: String::new(),
            logical_block_size: 512,
            physical_block_size: 4096,
            restore_mode: String::new(),
        };
        let sources = std::slice::from_ref(source);
        assert_eq!(
            restore_snapshot_block_contract(
                sources,
                chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh,
                std::slice::from_ref(&saved),
                None,
            )
            .unwrap(),
            [saved]
        );
        // A paired capture, which no restored microVM can take, fails instead
        // of flushing a file that it does not have.
        let err = capture_snapshot_block_contract(
            sources,
            chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired,
            Some([0x42; 16]),
            "",
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "microVM scratch block has no file");
    }

    /// Returns the source of a 1-MiB block of `role` with `file`.
    fn block_source(
        role: MicrovmSandboxBlockRole,
        file: Option<std::fs::File>,
    ) -> MicrovmSandboxBlockSource {
        MicrovmSandboxBlockSource {
            role,
            read_only: role.is_read_only(),
            length: MIB,
            logical_block_size: 512,
            physical_block_size: 4096,
            file,
        }
    }

    #[test]
    fn block_contracts_carry_the_storage_generation_without_reading_blocks() {
        use chipset_resources::microvm::MicrovmSnapshotScratchPolicy;
        const GENERATION: [u8; 16] = [0x42; 16];
        // The layer has no file, so neither contract can read it; the paired
        // scratch has one only for its capture to flush.
        let directory = tempfile::tempdir().unwrap();
        let scratch = std::fs::File::create_new(directory.path().join("scratch.img")).unwrap();
        scratch.set_len(MIB).unwrap();
        let capture_sources = [
            block_source(MicrovmSandboxBlockRole::Distro, None),
            block_source(MicrovmSandboxBlockRole::Scratch, Some(scratch)),
        ];

        // A tier that binds the blocks records the generation as the identity
        // of the layer and of the paired scratch.
        let bound = capture_snapshot_block_contract(
            &capture_sources,
            MicrovmSnapshotScratchPolicy::Paired,
            Some(GENERATION),
            "private-copy",
        )
        .unwrap();
        for block in &bound {
            assert_eq!(block.identity_kind, "generation");
            assert_eq!(block.identity, GENERATION);
        }
        assert_eq!(
            (bound[0].artifact.as_str(), bound[0].restore_mode.as_str()),
            ("", "")
        );
        assert_eq!(
            (bound[1].artifact.as_str(), bound[1].restore_mode.as_str()),
            ("scratch.img", "private-copy")
        );

        // The platform tier binds none: its layer is unbound, and its scratch
        // fresh.
        let unbound = capture_snapshot_block_contract(
            &capture_sources,
            MicrovmSnapshotScratchPolicy::Fresh,
            None,
            "private-copy",
        )
        .unwrap();
        assert_eq!(
            unbound
                .iter()
                .map(|block| (block.identity_kind.as_str(), block.identity.as_slice()))
                .collect::<Vec<_>>(),
            [("unbound", &[][..]), ("fresh", &[][..])]
        );
        assert!(unbound[1].artifact.is_empty() && unbound[1].restore_mode.is_empty());
        let err = capture_snapshot_block_contract(
            &capture_sources,
            MicrovmSnapshotScratchPolicy::Paired,
            None,
            "",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "microVM paired scratch requires a storage generation"
        );

        // A restore expects the bound blocks to have its own generation, so a
        // snapshot of the same storage matches, and one of other storage does
        // not.
        let restore_sources = [
            block_source(MicrovmSandboxBlockRole::Distro, None),
            block_source(MicrovmSandboxBlockRole::Scratch, None),
        ];
        let restore = |generation| {
            restore_snapshot_block_contract(
                &restore_sources,
                MicrovmSnapshotScratchPolicy::Paired,
                &bound,
                generation,
            )
        };
        assert_eq!(restore(Some(GENERATION)).unwrap(), bound);
        assert_ne!(restore(Some([0x24; 16])).unwrap(), bound);
        assert_eq!(
            restore(None).unwrap_err().to_string(),
            "snapshot binds the microVM distro block to a storage generation, which the restore requires"
        );
        assert_eq!(
            restore_snapshot_block_contract(
                &restore_sources,
                MicrovmSnapshotScratchPolicy::Fresh,
                &unbound,
                None,
            )
            .unwrap(),
            unbound
        );
    }
}
