// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM machine profile support for the OpenVMM entry point.

mod config;
mod console;
mod filesystem;
mod launch;
mod network;
pub(crate) mod output;
pub(crate) mod report;
mod restore;
mod verify;

pub(crate) use config::MicrovmConfigBuilder;
#[cfg(test)]
pub(crate) use console::MICROVM_CONSOLE_ATTACHMENT_KIND;
#[cfg(test)]
pub(crate) use console::MICROVM_CONSOLE_STABLE_ID;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::MICROVM_CONTROL_CONSOLE_STABLE_ID;
pub(crate) use console::MicrovmConsoleSocketCleanup;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::microvm_console_attachment_from_cli;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::microvm_console_attachment_from_snapshot;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::microvm_console_socket_cleanup;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::validate_microvm_console_attachment_namespace;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use filesystem::microvm_filesystem_attachment;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use filesystem::microvm_filesystem_from_snapshot;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use filesystem::microvm_filesystem_slot_from_snapshot;
pub(crate) use filesystem::validate_microvm_filesystem_private_storage;
pub(crate) use launch::MicrovmLaunch;
pub(crate) use restore::ExpectedRestoreContract;
pub(crate) use restore::MicrovmRestore;
pub(crate) use restore::TimeAbiRestore;
pub(crate) use restore::TimeAbiRestoreOptions;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use restore::fresh_microvm_generation_id;
pub(crate) use restore::prepare_restore;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use restore::restore_packet_base;
pub(crate) use restore::validate_restore_contract;
pub(crate) use verify::fatal_error_message;
pub(crate) use verify::report_time_abi_verification;

use crate::storage_builder::microvm::MicrovmSandboxBlockSource;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use net_backend_resources::egress::EgressPolicy;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use output::MicrovmOutputDrain;
use std::path::PathBuf;

/// Host-side microVM resources produced while building the VM configuration
/// and consumed by the snapshot, restore, and teardown paths.
#[derive(Default)]
pub(crate) struct MicrovmResources {
    /// Drains portb and its host output relay before a guest-requested exit.
    pub(crate) output_drain: Option<MicrovmOutputDrain>,
    /// Guest-requested snapshot boundaries from the snapshot-request port.
    pub(crate) snapshot_requests: Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
    /// Snapshot sources of the fixed-role sandbox blocks, in role order.
    pub(crate) sandbox_block_sources: Vec<MicrovmSandboxBlockSource>,
    /// Snapshot identity of the boot virtio-console endpoint.
    pub(crate) console_attachment: Option<SnapshotAttachment>,
    /// Removes the boot console Unix socket on teardown.
    pub(crate) console_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    /// Snapshot identity of the control virtio-console endpoint.
    pub(crate) control_console_attachment: Option<SnapshotAttachment>,
    /// Removes the control console Unix socket on teardown.
    pub(crate) control_console_socket_cleanup: Option<MicrovmConsoleSocketCleanup>,
    /// Snapshot identity of the portable network attachment.
    pub(crate) network_attachment: Option<SnapshotAttachment>,
    /// The bound run-scoped egress policy.
    pub(crate) egress_policy: Option<EgressPolicy>,
    /// Snapshot identities of the live filesystem roots, in virtio-fs slot
    /// order.
    pub(crate) filesystem_attachments: Vec<SnapshotAttachment>,
    /// Canonical host paths of the live filesystem roots, in virtio-fs slot
    /// order.
    pub(crate) filesystem_root_paths: Vec<PathBuf>,
    /// Under the time ABI, seals the time fields of the restore packet; the
    /// restoring worker takes it.
    pub(crate) restore_time_record:
        Option<mesh::OneshotSender<chipset_resources::microvm_time::RestoreTimeRecord>>,
    /// Under the time ABI with profiling enabled, notified when the guest
    /// first selects the restore packet; the restoring worker takes it.
    pub(crate) restore_packet_selected: Option<mesh::OneshotReceiver<()>>,
}

impl MicrovmResources {
    /// Returns the portb source of a time ABI restore packet with `base`, and
    /// keeps the restoring worker's ends of its channels.
    pub(crate) fn time_abi_restore_packet(
        &mut self,
        base: chipset_resources::microvm_time::RestorePacketBase,
    ) -> chipset_resources::microvm::MicrovmRestorePacketSource {
        let (record, time) = mesh::oneshot();
        self.restore_time_record = Some(record);
        let selected = openvmm_defs::profile::enabled().then(|| {
            let (selected, recv) = mesh::oneshot();
            self.restore_packet_selected = Some(recv);
            selected
        });
        chipset_resources::microvm::MicrovmRestorePacketSource {
            base,
            time,
            selected,
        }
    }
}

/// Configures the chipset devices of a microVM for the time ABI: puts the PIT
/// in strict mode, which forbids a periodic channel 0 at capture and restore
/// (`E_PIT_ACTIVE`), and returns the portb device's time ABI configuration,
/// with the restore packet of a restore.
pub(crate) fn time_abi_chipset(
    chipset_devices: &mut [vmotherboard::ChipsetDeviceHandle],
    hooks: &virt::time_abi::TimeAbiTestHooks,
    restore: Option<chipset_resources::microvm::MicrovmRestorePacketSource>,
) -> chipset_resources::microvm::MicrovmPortbTimeAbi {
    use vm_resource::IntoResource;
    use vm_resource::ResourceId;

    for device in chipset_devices.iter_mut() {
        if device.name == chipset_resources::pit::PitDeviceHandle::ID {
            device.resource =
                chipset_resources::pit::PitDeviceHandle { time_abi: true }.into_resource();
        }
    }
    chipset_resources::microvm::MicrovmPortbTimeAbi {
        generation: restore
            .as_ref()
            .map_or(0, |restore| restore.base.generation),
        utc_offset_ms: hooks.utc_offset_ms,
        sample_delay_us: hooks.sample_delay_us,
        test_hooks: hooks.active(),
        restore,
    }
}
