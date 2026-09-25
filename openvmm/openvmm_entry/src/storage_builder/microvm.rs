// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-role microVM sandbox block devices.

use super::StorageBuilder;
use super::VirtioBlkDisk;
use crate::VmResources;
use crate::cli_args::DiskCliKind;
use crate::disk_open;
use anyhow::Context;
use mesh::CellUpdater;
use openvmm_defs::config::Config;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmSandboxBlockConfig;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use std::time::Duration;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::IntoResource;
use vm_resource::Resource;

/// The fixed role of a microVM sandbox block device.
pub(super) struct SandboxBlock {
    role: MicrovmSandboxBlockRole,
    snapshot_source: Option<MicrovmSandboxBlockSource>,
}

pub(crate) struct MicrovmSandboxBlockSource {
    pub(crate) role: MicrovmSandboxBlockRole,
    pub(crate) read_only: bool,
    pub(crate) length: u64,
    pub(crate) logical_block_size: u32,
    pub(crate) physical_block_size: u32,
    pub(crate) file: std::fs::File,
}

pub(crate) fn snapshot_block_contract(
    sources: &[MicrovmSandboxBlockSource],
    scratch_policy: chipset_resources::microvm::MicrovmSnapshotScratchPolicy,
) -> anyhow::Result<Vec<openvmm_helpers::snapshot::microvm::SnapshotMicrovmSandboxBlock>> {
    sources
        .iter()
        .map(|source| {
            if !source.read_only
                && scratch_policy
                    == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Paired
            {
                source.file.sync_all().with_context(|| {
                    format!("failed to flush microVM {} block", source.role.as_str())
                })?;
            }
            let (identity_kind, identity, artifact) = if source.role
                == MicrovmSandboxBlockRole::Scratch
                && scratch_policy == chipset_resources::microvm::MicrovmSnapshotScratchPolicy::Fresh
            {
                ("fresh", Vec::new(), String::new())
            } else {
                (
                    "sha256",
                    openvmm_helpers::snapshot::fs::file_sha256(
                        &source.file,
                        source.length,
                        &format!("microVM {} block", source.role.as_str()),
                    )?,
                    if source.role == MicrovmSandboxBlockRole::Scratch {
                        openvmm_helpers::snapshot::format::SCRATCH_FILE_NAME.to_owned()
                    } else {
                        String::new()
                    },
                )
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
                },
            )
        })
        .collect()
}

impl StorageBuilder {
    /// Adds a fixed-role microVM sandbox block device.
    pub async fn add_microvm_sandbox_block(
        &mut self,
        role: MicrovmSandboxBlockRole,
        kind: &DiskCliKind,
        read_only: bool,
        snapshot_capable: bool,
    ) -> anyhow::Result<()> {
        if !snapshot_capable {
            self.vtl0_virtio_blk_disks.push(VirtioBlkDisk {
                disk: disk_open(kind, read_only).await?,
                read_only,
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
        let disk_file = file
            .try_clone()
            .context("failed to duplicate microVM sandbox block handle")?;
        #[cfg(target_os = "linux")]
        let disk = Resource::new(disk_backend_resources::BlockDeviceDiskHandle { file: disk_file });
        #[cfg(not(target_os = "linux"))]
        let disk = Resource::new(disk_backend_resources::FileDiskHandle(disk_file));
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
            microvm: Some(SandboxBlock {
                role,
                snapshot_source: Some(MicrovmSandboxBlockSource {
                    role,
                    read_only,
                    length,
                    logical_block_size,
                    physical_block_size,
                    file,
                }),
            }),
        });
        Ok(())
    }

    /// Adds the sandbox block devices of a microVM at their fixed virtio-mmio
    /// slots, in role order.
    pub(super) fn build_microvm_sandbox_blocks(
        &mut self,
        config: &mut Config,
        resources: &mut VmResources,
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
                    resources.microvm.sandbox_block_sources.push(source);
                }
            }
            config.virtio_devices.push((
                VirtioBus::Mmio,
                VirtioBlkHandle {
                    disk: vblk.disk,
                    read_only: vblk.read_only,
                }
                .into_resource(),
            ));
        }
        Ok(())
    }
}
