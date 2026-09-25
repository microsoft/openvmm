// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Fixed-role microVM sandbox block devices.

use super::StorageBuilder;
use super::VirtioBlkDisk;
use crate::cli_args::DiskCliKind;
use crate::disk_open;
use openvmm_defs::config::Config;
use openvmm_defs::config::VirtioBus;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::MicrovmSandboxBlockConfig;
use openvmm_defs::microvm::MicrovmSandboxBlockRole;
use virtio_resources::blk::VirtioBlkHandle;
use vm_resource::IntoResource;

/// The fixed role of a microVM sandbox block device.
pub(super) struct SandboxBlock {
    role: MicrovmSandboxBlockRole,
}

impl StorageBuilder {
    /// Adds a fixed-role microVM sandbox block device.
    pub async fn add_microvm_sandbox_block(
        &mut self,
        role: MicrovmSandboxBlockRole,
        kind: &DiskCliKind,
        read_only: bool,
    ) -> anyhow::Result<()> {
        self.vtl0_virtio_blk_disks.push(VirtioBlkDisk {
            disk: disk_open(kind, read_only).await?,
            read_only,
            microvm: Some(SandboxBlock { role }),
        });
        Ok(())
    }

    /// Adds the sandbox block devices of a microVM at their fixed virtio-mmio
    /// slots, in role order.
    pub(super) fn build_microvm_sandbox_blocks(
        &mut self,
        config: &mut Config,
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
