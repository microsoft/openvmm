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
pub(crate) use console::MICROVM_CONSOLE_TCP_REFUSED;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND;
#[cfg(any(feature = "ttrpc", feature = "grpc"))]
pub(crate) use console::MICROVM_CONTROL_CONSOLE_STABLE_ID;
pub(crate) use console::MicrovmConsoleSocketCleanup;
#[cfg(all(test, any(feature = "ttrpc", feature = "grpc")))]
pub(crate) use console::microvm_console_attachment_from_snapshot;
#[cfg(all(test, any(feature = "ttrpc", feature = "grpc")))]
pub(crate) use console::microvm_console_disconnect_policy;
pub(crate) use filesystem::validate_microvm_filesystem_private_storage;
pub(crate) use launch::MicrovmLaunch;
pub(crate) use restore::ExpectedRestoreContract;
pub(crate) use restore::MicrovmRestore;
pub(crate) use restore::TimeAbiRestore;
pub(crate) use restore::TimeAbiRestoreOptions;
pub(crate) use restore::prepare_restore;
pub(crate) use restore::validate_restore_contract;
pub(crate) use verify::fatal_error_message;
pub(crate) use verify::report_time_abi_verification;

use crate::storage_builder::microvm::MicrovmSandboxBlockSource;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use net_backend_resources::egress::EgressPolicy;
use openvmm_defs::config::LinuxDirectBootMode;
use openvmm_defs::microvm::MachineProfile;
use openvmm_helpers::snapshot::microvm::SnapshotAttachment;
use output::MicrovmOutputDrain;
use std::path::Path;
use std::path::PathBuf;

/// Resolves the profile-owned Linux boot description before machine admission.
pub(crate) fn linux_direct_boot_mode(
    profile: MachineProfile,
    standard_mode: LinuxDirectBootMode,
) -> LinuxDirectBootMode {
    match profile {
        MachineProfile::Standard => standard_mode,
        MachineProfile::Microvm => {
            #[cfg(guest_arch = "x86_64")]
            {
                LinuxDirectBootMode::MpTable
            }
            #[cfg(guest_arch = "aarch64")]
            {
                LinuxDirectBootMode::DeviceTree
            }
        }
    }
}

/// Profile-owned topology configuration shared by CLI and RPC construction.
pub(crate) fn topology_arch() -> openvmm_defs::config::ArchTopologyConfig {
    #[cfg(guest_arch = "x86_64")]
    {
        openvmm_defs::config::ArchTopologyConfig::X86(openvmm_defs::config::X86TopologyConfig {
            apic_id_offset: 0,
            x2apic: openvmm_defs::config::X2ApicConfig::Unsupported,
        })
    }
    #[cfg(guest_arch = "aarch64")]
    {
        openvmm_defs::config::ArchTopologyConfig::Aarch64(
            openvmm_defs::config::Aarch64TopologyConfig {
                gic_config: Some(openvmm_defs::config::GicConfig::V3(Some(
                    openvmm_defs::config::GicV3Config {
                        gic_distributor_base:
                            openvmm_defs::microvm::MICROVM_ARM64_GIC_DISTRIBUTOR_BASE,
                        gic_redistributors_base:
                            openvmm_defs::microvm::MICROVM_ARM64_GIC_REDISTRIBUTORS_BASE,
                    },
                ))),
                pmu_gsiv: openvmm_defs::config::PmuGsivConfig::Disabled,
                gic_msi: openvmm_defs::config::GicMsiConfig::Auto,
            },
        )
    }
}

/// Installs the architecture frontend over the same platform-control resources.
pub(crate) fn add_platform_chipset_devices(
    devices: &mut Vec<vmotherboard::ChipsetDeviceHandle>,
    portb: chipset_resources::microvm::MicrovmPortbHandle,
    snapshot_notify: Option<mesh::Sender<MicrovmSnapshotBoundaryRequest>>,
) {
    use vm_resource::IntoResource;
    use vm_resource::ResourceId;

    #[cfg(guest_arch = "x86_64")]
    {
        use chipset_resources::microvm::MicrovmPortbHandle;
        use chipset_resources::microvm::MicrovmShutdownHandle;
        use chipset_resources::microvm::MicrovmSnapshotRequestHandle;

        devices.extend([
            vmotherboard::ChipsetDeviceHandle {
                name: MicrovmPortbHandle::ID.to_owned(),
                resource: portb.into_resource(),
            },
            vmotherboard::ChipsetDeviceHandle {
                name: MicrovmShutdownHandle::ID.to_owned(),
                resource: MicrovmShutdownHandle.into_resource(),
            },
            vmotherboard::ChipsetDeviceHandle {
                name: MicrovmSnapshotRequestHandle::ID.to_owned(),
                resource: MicrovmSnapshotRequestHandle {
                    notify: snapshot_notify,
                }
                .into_resource(),
            },
        ]);
    }
    #[cfg(guest_arch = "aarch64")]
    {
        use chipset_resources::microvm::MicrovmPlatformControlHandle;

        drop(snapshot_notify);
        devices.push(vmotherboard::ChipsetDeviceHandle {
            name: MicrovmPlatformControlHandle::ID.to_owned(),
            resource: MicrovmPlatformControlHandle { portb }.into_resource(),
        });
    }
}

/// The live host side of the microVM filesystem.
#[derive(Clone, Debug)]
pub(crate) struct MicrovmFilesystemHost {
    /// Snapshot identity of the filesystem's attachment.
    pub(crate) attachment: SnapshotAttachment,
    /// Canonical host root of a single directory; empty for an aggregate.
    pub(crate) root_path: PathBuf,
    /// Canonical host roots and live root attachments of an aggregate's
    /// children, in child order.
    pub(crate) children: Vec<(PathBuf, SnapshotAttachment)>,
}

impl MicrovmFilesystemHost {
    /// Returns every live host root of the filesystem.
    pub(crate) fn root_paths(&self) -> Vec<&Path> {
        if self.children.is_empty() {
            vec![self.root_path.as_path()]
        } else {
            self.children
                .iter()
                .map(|(root_path, _)| root_path.as_path())
                .collect()
        }
    }
}

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
    /// The egress policy that the snapshot contract records: the bound
    /// run-scoped policy, except at a restore with `--restore-network-policy`,
    /// where it is the complete deny-all of the template whose policy the
    /// restore replaces.
    pub(crate) egress_policy: Option<EgressPolicy>,
    /// The live host side of the attached filesystem.
    pub(crate) filesystem: Option<MicrovmFilesystemHost>,
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
/// with the restore packet of a restore. The VM worker provides the host
/// fields of the time samples, and applies the hooks that alter them.
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
        sample_delay_us: hooks.sample_delay_us,
        test_hooks: hooks.active(),
        restore,
    }
}

#[cfg(test)]
mod tests {
    use super::linux_direct_boot_mode;
    use openvmm_defs::config::LinuxDirectBootMode;
    use openvmm_defs::microvm::MachineProfile;
    use test_with_tracing::test;

    #[test]
    fn resolves_profile_owned_linux_boot_mode() {
        for requested in [
            LinuxDirectBootMode::Acpi,
            LinuxDirectBootMode::DeviceTree,
            LinuxDirectBootMode::MpTable,
        ] {
            assert_eq!(
                linux_direct_boot_mode(MachineProfile::Standard, requested),
                requested
            );
            #[cfg(guest_arch = "x86_64")]
            let expected = LinuxDirectBootMode::MpTable;
            #[cfg(guest_arch = "aarch64")]
            let expected = LinuxDirectBootMode::DeviceTree;
            assert_eq!(
                linux_direct_boot_mode(MachineProfile::Microvm, requested),
                expected
            );
        }
    }

    #[test]
    fn platform_frontend_inventory_uses_existing_profile_resources() {
        use vm_resource::IntoResource;

        let mut devices = Vec::new();
        super::add_platform_chipset_devices(
            &mut devices,
            chipset_resources::microvm::MicrovmPortbHandle {
                io: serial_core::resources::DisconnectedSerialBackendHandle.into_resource(),
                generation_id: [0x3c; 16],
                output_drain: None,
                time_abi: chipset_resources::microvm::MicrovmPortbTimeAbi {
                    generation: 0,
                    sample_delay_us: 0,
                    test_hooks: false,
                    restore: None,
                },
            },
            None,
        );
        let ids: Vec<_> = devices
            .iter()
            .map(|device| (device.name.as_str(), device.resource.id()))
            .collect();
        #[cfg(guest_arch = "x86_64")]
        assert_eq!(
            ids,
            [
                ("microvm-portb", "microvm-portb"),
                ("microvm-shutdown", "microvm-shutdown"),
                ("microvm-snapshot-request", "microvm-snapshot-request"),
            ]
        );
        #[cfg(guest_arch = "aarch64")]
        assert_eq!(
            ids,
            [("microvm-platform-control", "microvm-platform-control")]
        );
    }
}
