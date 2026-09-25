// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM-specific support for the management RPC endpoint.

use super::VmLifecycle;
use super::grpc_error;
use crate::cli_args::SerialConfigCli;
use crate::serial_io::connect::bind_serial_without_cleanup;
use crate::vm_controller::MicrovmController;
use crate::vm_controller::VmControllerEvent;
use anyhow::Context;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotBoundaryRequest;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use chipset_resources::microvm::MicrovmSnapshotScratchPolicy;
use memory_range::MemoryRange;
use openvmm_defs::config::ArchTopologyConfig;
use openvmm_defs::config::Config;
use openvmm_defs::config::LoadMode;
use openvmm_defs::config::MemoryConfig;
use openvmm_defs::config::NumaNode;
use openvmm_defs::config::NumaTopology;
use openvmm_defs::config::VpAssignment;
use openvmm_defs::config::X86TopologyConfig;
use openvmm_defs::microvm::MICROVM_ABI_VERSION_2;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::build_microvm_command_line;
use openvmm_defs::worker::SharedMemoryFd;
use openvmm_defs::worker::SnapshotRestoreGuards;
use openvmm_ttrpc_vmservice as vmservice;
use serial_core::resources::DisconnectedSerialBackendHandle;
use std::fs::File;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use vm_manifest_builder::BaseChipsetType;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VirtioDeviceHandle;
use vmotherboard::ChipsetDeviceHandle;

type RestoreTime = (Duration, u64, Option<u64>, Vec<u8>);
type SerialPorts = [Option<Resource<SerialBackendHandle>>; 4];

struct AuthoritativeRestore {
    path: PathBuf,
    snapshot: Option<openvmm_helpers::snapshot::restore::OpenedSnapshot>,
    base_memory_size: u64,
    selected_memory_size: u64,
    restore_memory_target_requested: bool,
    restore_memory_ranges: Vec<openvmm_helpers::snapshot::microvm::SnapshotMemoryExpansionRange>,
    vp_count: u32,
    machine_contract: openvmm_helpers::snapshot::microvm::SnapshotMachineContract,
}

struct RestoredFilesystem {
    config: openvmm_defs::microvm::MicrovmFilesystemConfig,
    root_path: String,
    attachment: openvmm_helpers::snapshot::microvm::SnapshotAttachment,
}

/// MicroVM state accumulated while a management-RPC VM is being created.
pub(super) struct CreateVm {
    active: bool,
    machine_profile: MachineProfile,
    processor_count: u32,
    authoritative_restore: Option<AuthoritativeRestore>,
    restore_console_config: Option<vmservice::VirtioConsoleConfig>,
    restore_filesystem_config: Option<vmservice::VirtioFs>,
    restored_filesystem: Option<RestoredFilesystem>,
    snapshot_destination: Option<PathBuf>,
    restore_entropy: bool,
    restore_online_vp_count: Option<u32>,
    restore_gate_timeout: Duration,
    snapshot_quiesce_timeout: Duration,
    memory_capacity_bytes: u64,
    restore_ready_path: String,
    source_hypervisor: String,
    shared_memory: Option<SharedMemoryFd>,
    snapshot_restore_guards: Option<SnapshotRestoreGuards>,
    saved_state: Option<mesh::payload::message::ProtobufMessage>,
    restore_time: Option<RestoreTime>,
    snapshot_notify: Option<mesh::Sender<MicrovmSnapshotBoundaryRequest>>,
    snapshot_ready: Option<mesh::Sender<MicrovmSnapshotScratchPolicy>>,
    snapshot_requests: Option<mesh::Receiver<MicrovmSnapshotScratchPolicy>>,
    portb: Option<Resource<SerialBackendHandle>>,
    portb_has_output: bool,
    resources: crate::microvm::MicrovmResources,
    filesystem_slot: bool,
    filesystem: Option<openvmm_defs::microvm::MicrovmFilesystemConfig>,
    effective_command_line: Option<String>,
    snapshot_memory_file: Option<tempfile::NamedTempFile>,
    snapshot_memory_handle: Option<File>,
    snapshot_memory_path: Option<PathBuf>,
    memory_capacity: Option<u64>,
}

impl CreateVm {
    /// Parses the microVM portion of a create request and returns the effective
    /// VM configuration.
    pub(super) fn new(
        request: vmservice::CreateVmRequest,
    ) -> anyhow::Result<(Self, vmservice::VmConfig)> {
        let vmservice::CreateVmRequest {
            config: requested_config,
            microvm_snapshot,
            ..
        } = request;
        let vmservice::MicrovmSnapshotConfig {
            destination_path,
            restore_path,
            restore_entropy,
            quiesce_timeout_ms,
            restore_ready_path,
            restore_processor_count,
            restore_gate_timeout_ms,
            memory_capacity_bytes,
            restore_memory_bytes,
        } = microvm_snapshot.unwrap_or_default();
        let restore_online_vp_count =
            (restore_processor_count != 0).then_some(restore_processor_count);
        let restore_gate_timeout = Duration::from_millis(if restore_gate_timeout_ms == 0 {
            60_000
        } else {
            restore_gate_timeout_ms
        });
        let resolve_path = |value: String| -> anyhow::Result<Option<PathBuf>> {
            if value.is_empty() {
                return Ok(None);
            }
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(Some(path))
            } else {
                Ok(Some(
                    std::env::current_dir()
                        .context("failed to resolve current directory")?
                        .join(path),
                ))
            }
        };
        let snapshot_destination = resolve_path(destination_path)?;
        let restore_path = resolve_path(restore_path)?;
        anyhow::ensure!(
            snapshot_destination.is_none() || restore_path.is_none(),
            "snapshot capture and restore paths are mutually exclusive"
        );
        anyhow::ensure!(
            !restore_entropy || restore_path.is_some(),
            "restore_entropy requires restore_path"
        );
        anyhow::ensure!(
            restore_online_vp_count.is_none() || restore_path.is_some(),
            "restore_processor_count requires restore_path"
        );
        anyhow::ensure!(
            restore_gate_timeout_ms == 0 || restore_path.is_some(),
            "restore_gate_timeout_ms requires restore_path"
        );
        anyhow::ensure!(
            memory_capacity_bytes == 0 || snapshot_destination.is_some(),
            "memory_capacity_bytes requires destination_path"
        );
        anyhow::ensure!(
            restore_memory_bytes == 0 || restore_path.is_some(),
            "restore_memory_bytes requires restore_path"
        );
        anyhow::ensure!(
            restore_ready_path.is_empty() || restore_path.is_some(),
            "restore_ready_path requires restore_path"
        );
        anyhow::ensure!(
            quiesce_timeout_ms == 0 || snapshot_destination.is_some(),
            "quiesce_timeout_ms requires destination_path"
        );
        let snapshot_quiesce_timeout = Duration::from_millis(if quiesce_timeout_ms == 0 {
            5_000
        } else {
            quiesce_timeout_ms
        });

        let authoritative_restore = if let Some(path) = restore_path {
            let snapshot = openvmm_helpers::snapshot::restore::OpenedSnapshot::open(&path)?;
            let manifest = snapshot.manifest();
            openvmm_helpers::snapshot::validate_manifest(
                manifest,
                crate::GUEST_ARCH,
                manifest.memory_size_bytes,
                manifest.vp_count,
                crate::system_page_size(),
            )?;
            let machine_contract = manifest
                .machine_contract
                .as_ref()
                .context("microVM snapshot is missing its authoritative machine contract")?;
            anyhow::ensure!(
                machine_contract.machine_profile == "microvm",
                "snapshot machine profile is not microvm"
            );
            openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(
                machine_contract,
            )?;
            validate_restore_console_attachments(&machine_contract.attachments)?;
            anyhow::ensure!(
                machine_contract.microvm_sandbox_blocks.is_empty(),
                "TTRPC restore does not support microVM sandbox-block snapshots"
            );
            let base_memory_size = manifest.memory_size_bytes;
            let restore_memory_target_requested = restore_memory_bytes != 0;
            let selected_memory_size = if restore_memory_target_requested {
                restore_memory_bytes
            } else {
                base_memory_size
            };
            let restore_memory_ranges = if restore_memory_target_requested {
                openvmm_helpers::snapshot::microvm::validate_restore_memory_target(
                    manifest,
                    selected_memory_size,
                )?
            } else {
                Vec::new()
            };
            let vp_count = manifest.vp_count;
            if let Some(restore_online_vp_count) = restore_online_vp_count {
                openvmm_helpers::snapshot::microvm::validate_restore_online_vp_count(
                    manifest,
                    restore_online_vp_count,
                )?;
            }
            let machine_contract = machine_contract.clone();
            Some(AuthoritativeRestore {
                path,
                snapshot: Some(snapshot),
                base_memory_size,
                selected_memory_size,
                restore_memory_target_requested,
                restore_memory_ranges,
                vp_count,
                machine_contract,
            })
        } else {
            None
        };

        let mut restore_console_config = None;
        let mut restore_filesystem_config = None;
        let req_config = if let Some(restore) = &authoritative_restore {
            let restore_profile =
                ttrpc_machine_profile(restore.machine_contract.microvm_abi_version)?;
            let requested = requested_config.unwrap_or_else(|| vmservice::VmConfig {
                machine_profile: restore_profile as i32,
                ..Default::default()
            });
            anyhow::ensure!(
                requested.machine_profile == restore_profile as i32,
                "microVM snapshot restore requires the matching microVM ABI profile"
            );
            anyhow::ensure!(
                requested.memory_config.is_none()
                    && requested.boot_config.is_none()
                    && requested.windows_options.is_none()
                    && requested.hvsocket_config.is_none()
                    && requested.numa_config.is_none()
                    && requested.pcie.is_none(),
                "restore configuration may contain only processor identity, serial attachments, and guest power actions"
            );
            if let Some(processors) = &requested.processor_config {
                anyhow::ensure!(
                    processors.processor_count == restore.vp_count
                        && processors.processor_weight == 0
                        && processors.processor_limit == 0,
                    "restore processor count does not match the snapshot"
                );
            }
            if let Some(devices) = &requested.devices_config {
                anyhow::ensure!(
                    devices.scsi_disks.is_empty()
                        && devices.vpmem_disks.is_empty()
                        && devices.nic_config.is_empty()
                        && devices.windows_device.is_empty()
                        && devices.virtiofs_config.len() <= 1,
                    "restore devices_config may contain only one virtio-fs and one virtio-console attachment"
                );
                restore_console_config = devices.virtio_console.clone();
                restore_filesystem_config = devices.virtiofs_config.first().cloned();
            }
            vmservice::VmConfig {
                serial_config: requested.serial_config,
                guest_power_actions: requested.guest_power_actions,
                machine_profile: restore_profile as i32,
                boot_config: Some(vmservice::vm_config::BootConfig::DirectBoot(
                    Default::default(),
                )),
                ..Default::default()
            }
        } else {
            requested_config.context("missing configuration")?
        };

        let requested_profile =
            vmservice::vm_config::MachineProfile::from_i32(req_config.machine_profile)
                .with_context(|| {
                    format!("unknown machine profile {}", req_config.machine_profile)
                })?;
        let machine_profile = openvmm_machine_profile(requested_profile);
        let active = machine_profile == MachineProfile::Microvm;
        let processor_count = authoritative_restore
            .as_ref()
            .map(|restore| restore.vp_count)
            .or_else(|| {
                req_config
                    .processor_config
                    .as_ref()
                    .map(|config| config.processor_count)
            })
            .unwrap_or(1);
        anyhow::ensure!(
            active || (snapshot_destination.is_none() && authoritative_restore.is_none()),
            "microVM snapshot options require the microVM machine profile"
        );

        Ok((
            Self {
                active,
                machine_profile,
                processor_count,
                authoritative_restore,
                restore_console_config,
                restore_filesystem_config,
                restored_filesystem: None,
                snapshot_destination,
                restore_entropy,
                restore_online_vp_count,
                restore_gate_timeout,
                snapshot_quiesce_timeout,
                memory_capacity_bytes,
                restore_ready_path,
                source_hypervisor: String::new(),
                shared_memory: None,
                snapshot_restore_guards: None,
                saved_state: None,
                restore_time: None,
                snapshot_notify: None,
                snapshot_ready: None,
                snapshot_requests: None,
                portb: None,
                portb_has_output: false,
                resources: Default::default(),
                filesystem_slot: active,
                filesystem: None,
                effective_command_line: None,
                snapshot_memory_file: None,
                snapshot_memory_handle: None,
                snapshot_memory_path: None,
                memory_capacity: None,
            },
            req_config,
        ))
    }

    /// Completes restore preparation after the hypervisor backend has been
    /// selected.
    pub(super) fn prepare_restore(&mut self, source_hypervisor: String) -> anyhow::Result<()> {
        self.source_hypervisor = source_hypervisor;
        if let Some(restore) = &self.authoritative_restore {
            anyhow::ensure!(
                restore.machine_contract.source_hypervisor == self.source_hypervisor,
                "snapshot source hypervisor '{}' does not match destination '{}'",
                restore.machine_contract.source_hypervisor,
                self.source_hypervisor
            );
            self.filesystem_slot =
                crate::microvm::microvm_filesystem_slot_from_snapshot(&restore.machine_contract)?;
        }

        self.restored_filesystem = if let Some(restore) = &self.authoritative_restore {
            let saved_policy = restore.machine_contract.microvm_filesystem.as_ref();
            let saved_attachment = restore
                .machine_contract
                .attachments
                .iter()
                .find(|attachment| attachment.stable_id == "fs:microvm0");
            anyhow::ensure!(
                saved_policy.is_some() == saved_attachment.is_some()
                    && (restore.machine_contract.microvm_filesystem_slot_version
                        == openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION
                        || self.filesystem_slot == saved_policy.is_some()),
                "snapshot microVM filesystem slot, policy, and attachment inventories disagree"
            );
            match (saved_policy, saved_attachment) {
                (Some(saved_policy), Some(saved_attachment)) => {
                    let requested = self.restore_filesystem_config.as_ref().context(
                        "snapshot restore requires a fresh virtiofs_config attachment for fs:microvm0",
                    )?;
                    let config = crate::microvm::microvm_filesystem_from_snapshot(saved_policy)?;
                    anyhow::ensure!(
                        requested.tag == "microvm"
                            && requested.guest_mount_target == config.guest_mount_target
                            && requested.read_write != config.access.is_read_only()
                            && !requested.root_path.is_empty(),
                        "restore-time virtiofs_config does not match the snapshot policy"
                    );
                    let (root_path, attachment) = crate::microvm::microvm_filesystem_attachment(
                        Path::new(&requested.root_path),
                    )?;
                    anyhow::ensure!(
                        !saved_policy.canonical_host_path.is_empty(),
                        "snapshot filesystem canonical host path is missing; this snapshot predates path-bound filesystem restore"
                    );
                    anyhow::ensure!(
                        root_path == saved_policy.canonical_host_path,
                        "restore-time filesystem canonical host path does not match the snapshot contract"
                    );
                    anyhow::ensure!(
                        &attachment == saved_attachment,
                        "restore-time filesystem root identity does not match the snapshot attachment"
                    );
                    Some(RestoredFilesystem {
                        config,
                        root_path,
                        attachment,
                    })
                }
                (None, None) => {
                    if let Some(requested) = self.restore_filesystem_config.as_ref() {
                        anyhow::ensure!(
                            restore.machine_contract.microvm_filesystem_slot_version
                                == openvmm_helpers::snapshot::microvm::MICROVM_FILESYSTEM_SLOT_VERSION,
                            "snapshot does not support restore-time microVM filesystem attachment"
                        );
                        anyhow::ensure!(
                            requested.tag == "microvm" && !requested.root_path.is_empty(),
                            "restore-time virtiofs_config does not match the fixed microVM slot"
                        );
                        let config = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                            requested.guest_mount_target.clone(),
                            if requested.read_write {
                                openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
                            } else {
                                openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly
                            },
                        )?;
                        let (root_path, attachment) =
                            crate::microvm::microvm_filesystem_attachment(Path::new(
                                &requested.root_path,
                            ))?;
                        Some(RestoredFilesystem {
                            config,
                            root_path,
                            attachment,
                        })
                    } else {
                        None
                    }
                }
                _ => anyhow::bail!(
                    "snapshot microVM filesystem policy and attachment inventories disagree"
                ),
            }
        } else {
            None
        };

        if let Some(restore) = &mut self.authoritative_restore {
            anyhow::ensure!(
                restore.machine_contract.microvm_network.is_none(),
                "ttrpc restore does not yet expose microVM network attachments"
            );
            let prepared = crate::snapshot_restore::prepare_snapshot_restore_for_config(
                restore
                    .snapshot
                    .take()
                    .context("snapshot restore is missing its opened generation")?,
                restore.base_memory_size,
                restore.selected_memory_size,
                restore.vp_count,
                Some((
                    &self.source_hypervisor,
                    &restore.machine_contract.effective_command_line,
                    None,
                    self.restored_filesystem.as_ref().map(|filesystem| {
                        (
                            &filesystem.config,
                            Path::new(&filesystem.root_path),
                            &filesystem.attachment,
                        )
                    }),
                    restore
                        .machine_contract
                        .attachments
                        .iter()
                        .find(|attachment| attachment.stable_id == "console:microvm-virtio0"),
                    None,
                    restore.machine_contract.microvm_sandbox_blocks.clone(),
                )),
            )?;
            let restore_time = prepared
                .restore_time
                .context("microVM snapshot is missing its restore-time contract")?;
            if !self.restore_entropy
                && self.restore_online_vp_count.is_none()
                && !restore.restore_memory_target_requested
            {
                tracing::warn!(
                    "restoring cloned guest RNG state without fresh entropy injection; cryptographic workloads are unsafe"
                );
            }
            self.shared_memory = Some(prepared.shared_memory);
            self.snapshot_restore_guards = Some(prepared.guards);
            self.saved_state = Some(prepared.saved_state);
            self.restore_time = Some(restore_time);
        }
        Ok(())
    }

    /// Takes the process-local restore readiness endpoint path.
    pub(super) fn take_restore_ready_path(&mut self) -> String {
        std::mem::take(&mut self.restore_ready_path)
    }

    /// Creates snapshot coordination channels and validates the effective
    /// microVM request.
    pub(super) fn finish_prepare(
        &mut self,
        req_config: &vmservice::VmConfig,
    ) -> anyhow::Result<()> {
        if self.active {
            let (notify, requests) = mesh::channel();
            self.snapshot_notify = Some(notify);
            self.resources.snapshot_requests = Some(requests);
            let (ready, requests) = mesh::channel();
            self.snapshot_ready = Some(ready);
            self.snapshot_requests = Some(requests);
        }

        if self.active {
            anyhow::ensure!(
                cfg!(guest_arch = "x86_64"),
                "microVM requires an x86-64 guest"
            );
            if self.authoritative_restore.is_none() {
                anyhow::ensure!(
                    matches!(
                        req_config.boot_config.as_ref(),
                        Some(vmservice::vm_config::BootConfig::DirectBoot(_))
                    ),
                    "the microVM profile requires direct_boot"
                );
            }
            anyhow::ensure!(
                openvmm_defs::microvm::microvm_processor_count_supported(self.processor_count),
                "microVM does not support {} vCPUs",
                self.processor_count
            );
            anyhow::ensure!(
                req_config.numa_config.is_none(),
                "microVM does not support custom NUMA topology"
            );
            anyhow::ensure!(req_config.pcie.is_none(), "microVM does not support PCIe");
            anyhow::ensure!(
                req_config.iommufds.is_empty(),
                "microVM ABI version 1 does not support iommufd contexts"
            );
            anyhow::ensure!(
                req_config.hvsocket_config.is_none(),
                "microVM does not support hvsocket"
            );

            let serial_ports = req_config
                .serial_config
                .iter()
                .flat_map(|config| &config.ports)
                .collect::<Vec<_>>();
            anyhow::ensure!(
                serial_ports.len() <= 1 && serial_ports.iter().all(|port| port.port == 0),
                "microVM accepts only serial port 0 as its portb endpoint"
            );
            if let Some(devices) = &req_config.devices_config {
                anyhow::ensure!(
                    devices.scsi_disks.is_empty()
                        && devices.vpmem_disks.is_empty()
                        && devices.nic_config.is_empty()
                        && devices.windows_device.is_empty()
                        && devices.virtiofs_config.len() <= 1,
                    "microVM supports only one fixed virtio-fs and one fixed virtio-console device"
                );
                if let Some(filesystem) = devices.virtiofs_config.first() {
                    anyhow::ensure!(
                        !filesystem.read_only,
                        "microVM virtio-fs uses read_write rather than read_only"
                    );
                    anyhow::ensure!(
                        filesystem.tag == "microvm" && !filesystem.root_path.is_empty(),
                        "microVM virtio-fs requires tag 'microvm' and a host root path"
                    );
                    openvmm_defs::microvm::MicrovmFilesystemConfig::new(
                        filesystem.guest_mount_target.clone(),
                        if filesystem.read_write {
                            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
                        } else {
                            openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly
                        },
                    )?;
                }
                if let Some(console) = &devices.virtio_console {
                    anyhow::ensure!(
                        !console.socket_path.is_empty(),
                        "microVM virtio-console requires a socket path"
                    );
                }
            }
        }
        Ok(())
    }

    /// Returns whether the microVM machine profile is active.
    pub(super) fn is_active(&self) -> bool {
        self.active
    }

    /// Returns the effective virtual processor count.
    pub(super) fn processor_count(&self) -> u32 {
        self.processor_count
    }

    /// Returns whether a cold boot requested its fixed virtio-console.
    pub(super) fn has_requested_console(&self, config: &vmservice::VmConfig) -> bool {
        self.active
            && self.authoritative_restore.is_none()
            && config
                .devices_config
                .as_ref()
                .and_then(|devices| devices.virtio_console.as_ref())
                .is_some()
    }

    /// Creates the inert kernel handle used by an authoritative restore.
    pub(super) fn restore_kernel(&self) -> anyhow::Result<Option<File>> {
        if self.authoritative_restore.is_some() {
            return Ok(Some(
                tempfile::tempfile().context("failed to create inert restore kernel handle")?,
            ));
        }
        Ok(None)
    }

    /// Builds the effective direct-boot kernel command line.
    pub(super) fn command_line(
        &self,
        requested: String,
        has_requested_console: bool,
    ) -> anyhow::Result<String> {
        if let Some(restore) = &self.authoritative_restore {
            return Ok(restore.machine_contract.effective_command_line.clone());
        }
        if self.active {
            let mut cmdline = build_microvm_command_line(&[requested], has_requested_console)?;
            openvmm_defs::microvm::append_microvm_processor_limit(
                &mut cmdline,
                self.processor_count,
            )?;
            Ok(cmdline)
        } else {
            Ok(requested)
        }
    }

    /// Returns the direct-boot mode selected by the machine profile.
    pub(super) fn direct_boot_mode(&self) -> openvmm_defs::config::LinuxDirectBootMode {
        if self.active {
            openvmm_defs::config::LinuxDirectBootMode::MpTable
        } else {
            openvmm_defs::config::LinuxDirectBootMode::Acpi
        }
    }

    /// Returns the base chipset selected by the machine profile.
    pub(super) fn base_chipset_type(&self) -> BaseChipsetType {
        if self.active {
            BaseChipsetType::Microvm
        } else {
            BaseChipsetType::HyperVGen2LinuxDirect
        }
    }

    /// Removes the microVM portb endpoint from the standard serial-port array.
    pub(super) fn configure_serial_ports(
        &mut self,
        mut ports: SerialPorts,
    ) -> anyhow::Result<Option<SerialPorts>> {
        if !self.active {
            return Ok(Some(ports));
        }
        self.portb_has_output = ports[0].is_some();
        if ports.iter().skip(1).any(Option::is_some) {
            anyhow::bail!("microVM accepts only serial port 0 as its portb endpoint");
        }
        self.portb = Some(
            ports[0]
                .take()
                .unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
        );
        Ok(None)
    }

    /// Adds the fixed microVM chipset devices to the manifest.
    pub(super) fn add_chipset_devices(
        &mut self,
        devices: &mut Vec<ChipsetDeviceHandle>,
    ) -> anyhow::Result<()> {
        let Some(io) = self.portb.take() else {
            return Ok(());
        };
        let output_drain = self.portb_has_output.then(|| {
            let (drain, requests) = crate::microvm::output::MicrovmOutputDrain::new(None);
            self.resources.output_drain = Some(drain);
            requests
        });
        let restore_memory_ranges = self
            .authoritative_restore
            .as_ref()
            .map(|restore| restore.restore_memory_ranges.as_slice())
            .unwrap_or_default();
        let restore_memory_target_requested = self
            .authoritative_restore
            .as_ref()
            .is_some_and(|restore| restore.restore_memory_target_requested);
        let (generation_id, restore_entropy) = if self.restore_entropy
            || self.restore_online_vp_count.is_some()
            || restore_memory_target_requested
        {
            crate::microvm::fresh_microvm_restore_packet(
                self.restore_online_vp_count,
                restore_memory_target_requested,
                restore_memory_ranges,
            )?
        } else {
            (crate::microvm::fresh_microvm_generation_id()?, Vec::new())
        };
        devices.extend([
            ChipsetDeviceHandle {
                name: MicrovmPortbHandle::ID.to_owned(),
                resource: MicrovmPortbHandle {
                    io,
                    generation_id,
                    restore_entropy,
                    output_drain,
                }
                .into_resource(),
            },
            ChipsetDeviceHandle {
                name: MicrovmShutdownHandle::ID.to_owned(),
                resource: MicrovmShutdownHandle.into_resource(),
            },
            ChipsetDeviceHandle {
                name: MicrovmSnapshotRequestHandle::ID.to_owned(),
                resource: MicrovmSnapshotRequestHandle {
                    notify: self.snapshot_notify.take(),
                    input_gate_timeout: self.snapshot_quiesce_timeout,
                }
                .into_resource(),
            },
        ]);
        Ok(())
    }

    /// Returns the snapshot-selected NUMA topology for an authoritative
    /// restore.
    pub(super) fn restore_numa(&self) -> Option<(NumaTopology, u64)> {
        self.authoritative_restore.as_ref().map(|restore| {
            let mem_size = restore.selected_memory_size;
            let numa = NumaTopology {
                nodes: vec![NumaNode {
                    mem: Some(MemoryConfig {
                        mem_size,
                        prefetch_memory: false,
                        private_memory: false,
                        transparent_hugepages: true,
                        hugepages: false,
                        hugepage_size: None,
                        host_numa_node: None,
                    }),
                    vps: VpAssignment::FromTopology,
                }],
                distances: vec![],
            };
            (numa, mem_size)
        })
    }

    /// Applies machine-profile-specific values to an otherwise standard VM
    /// configuration.
    pub(super) fn apply_config(&self, config: &mut Config) {
        config.machine_profile = self.machine_profile;
        if self.active {
            config.processor_topology.vps_per_socket = Some(self.processor_count);
            config.processor_topology.enable_smt = Some(false);
            config.processor_topology.arch = Some(ArchTopologyConfig::X86(X86TopologyConfig {
                apic_id_offset: 0,
                x2apic: openvmm_defs::config::X2ApicConfig::Unsupported,
            }));
            config.hypervisor.with_hv = false;
            config.vmbus = None;
        }
        config.microvm.filesystem_bootstrap = self
            .authoritative_restore
            .as_ref()
            .is_some_and(|restore| restore.machine_contract.microvm_filesystem.is_some());
        config.microvm.memory_capacity = self
            .authoritative_restore
            .as_ref()
            .and_then(|restore| {
                (restore.machine_contract.memory_expansion_version != 0)
                    .then_some(restore.machine_contract.memory_capacity_bytes)
            })
            .or((self.memory_capacity_bytes != 0).then_some(self.memory_capacity_bytes));
        config.microvm.snapshot_memory_ranges = self
            .authoritative_restore
            .as_ref()
            .filter(|restore| restore.machine_contract.memory_expansion_version != 0)
            .map(|restore| {
                restore
                    .machine_contract
                    .memory_ranges
                    .iter()
                    .map(|range| MemoryRange::new(range.gpa_start..range.gpa_start + range.length))
                    .collect()
            })
            .unwrap_or_default();
        config.microvm.restore_memory_ranges = self
            .authoritative_restore
            .as_ref()
            .map(|restore| {
                restore
                    .restore_memory_ranges
                    .iter()
                    .map(|range| MemoryRange::new(range.gpa_start..range.gpa_start + range.length))
                    .collect()
            })
            .unwrap_or_default();
    }

    /// Adds devices reconstructed from an authoritative snapshot contract.
    pub(super) fn add_restored_devices(&mut self, config: &mut Config) -> anyhow::Result<()> {
        if let Some(filesystem) = self.restored_filesystem.take() {
            self.resources.filesystem_root_path = Some(PathBuf::from(&filesystem.root_path));
            config.microvm.filesystem = Some(filesystem.config.clone());
            config.virtio_devices.push((
                openvmm_defs::config::VirtioBus::Mmio,
                virtio_resources::fs::VirtioFsHandle {
                    tag: "microvm".to_owned(),
                    fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                        root_path: filesystem.root_path,
                        mount_options: String::new(),
                    },
                    profile: virtio_resources::fs::microvm::VirtioFsProfile::Microvm {
                        stable_id: "fs:microvm0".to_owned(),
                        root_identity: filesystem.attachment.identity.clone(),
                        read_only: filesystem.config.access.is_read_only(),
                        denied_paths: Vec::new(),
                    },
                }
                .into_resource(),
            ));
            self.resources.filesystem_attachment = Some(filesystem.attachment);
        }

        let Some(restore) = &self.authoritative_restore else {
            return Ok(());
        };
        let has_console = restore
            .machine_contract
            .devices
            .iter()
            .any(|device| device.stable_id == "console:microvm-virtio0");
        let attachment = restore
            .machine_contract
            .attachments
            .iter()
            .find(|attachment| attachment.stable_id == "console:microvm-virtio0");
        anyhow::ensure!(
            has_console == attachment.is_some(),
            "snapshot microVM console device and attachment inventories disagree"
        );
        if let Some(attachment) = attachment {
            let requested_attachment = self.restore_console_config.as_ref().map(|console| {
                if console.connect {
                    SerialConfigCli::ConnectPipe(PathBuf::from(&console.socket_path))
                } else {
                    SerialConfigCli::Pipe(PathBuf::from(&console.socket_path))
                }
            });
            let (endpoint_config, resource_attachment, snapshot_attachment) =
                crate::microvm::microvm_console_attachment_from_snapshot(
                    attachment,
                    requested_attachment.as_ref(),
                )?;
            crate::microvm::validate_microvm_console_attachment_namespace(
                &snapshot_attachment,
                &restore.path,
            )?;
            let (backend, disconnect_policy) = match endpoint_config {
                SerialConfigCli::Pipe(path) => {
                    let backend = bind_serial_without_cleanup(&path).with_context(|| {
                        format!(
                            "failed to recreate virtio console listener: {}",
                            path.display()
                        )
                    })?;
                    self.resources.console_socket_cleanup =
                        crate::microvm::microvm_console_socket_cleanup(path)?;
                    (backend, VirtioConsoleDisconnectPolicy::Retain)
                }
                SerialConfigCli::Tcp(address) => (
                    crate::serial_io::bind_tcp_serial(&address)?,
                    VirtioConsoleDisconnectPolicy::Retain,
                ),
                SerialConfigCli::ConnectPipe(path) => (
                    crate::serial_io::connect::connect_serial_with_timeout(
                        &path,
                        Duration::from_millis(
                            openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                        ),
                    )
                    .with_context(|| {
                        format!(
                            "failed to reconnect virtio console client: {}",
                            path.display()
                        )
                    })?,
                    VirtioConsoleDisconnectPolicy::Retain,
                ),
                SerialConfigCli::ConnectTcp(address) => (
                    crate::serial_io::connect::connect_tcp_serial(
                        &address,
                        Duration::from_millis(
                            openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                        ),
                    )?,
                    VirtioConsoleDisconnectPolicy::Retain,
                ),
                SerialConfigCli::None => (
                    DisconnectedSerialBackendHandle.into_resource(),
                    VirtioConsoleDisconnectPolicy::Discard,
                ),
                _ => unreachable!("saved microVM console was validated as an attachment"),
            };
            config.virtio_devices.push((
                openvmm_defs::config::VirtioBus::Mmio,
                virtio_resources::console::VirtioConsoleHandle {
                    backend,
                    disconnect_policy,
                    attachment: Some(resource_attachment),
                }
                .into_resource(),
            ));
            self.resources.console_attachment = Some(snapshot_attachment);
        }
        Ok(())
    }

    /// Consumes microVM-only devices so the caller can retain the unchanged
    /// standard-device construction path.
    pub(super) fn take_devices(
        &mut self,
        config: &mut Config,
        devices: &mut vmservice::DevicesConfig,
    ) -> anyhow::Result<()> {
        if !self.active {
            return Ok(());
        }
        for filesystem in std::mem::take(&mut devices.virtiofs_config) {
            self.add_filesystem(config, filesystem)?;
        }
        if let Some(console) = devices.virtio_console.take() {
            self.add_console(config, console)?;
        }
        Ok(())
    }

    fn add_filesystem(
        &mut self,
        config: &mut Config,
        filesystem: vmservice::VirtioFs,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            config.microvm.filesystem.is_none()
                && filesystem.tag == "microvm"
                && !filesystem.root_path.is_empty(),
            "microVM permits one virtio-fs attachment with tag 'microvm'"
        );
        let filesystem_config = openvmm_defs::microvm::MicrovmFilesystemConfig::new(
            filesystem.guest_mount_target,
            if filesystem.read_write {
                openvmm_defs::microvm::MicrovmFilesystemAccess::ReadWrite
            } else {
                openvmm_defs::microvm::MicrovmFilesystemAccess::ReadOnly
            },
        )?;
        let (root_path, attachment) =
            crate::microvm::microvm_filesystem_attachment(Path::new(&filesystem.root_path))?;
        self.resources.filesystem_root_path = Some(PathBuf::from(&root_path));
        let resource = virtio_resources::fs::VirtioFsHandle {
            tag: "microvm".to_owned(),
            fs: virtio_resources::fs::VirtioFsBackend::HostFs {
                root_path,
                mount_options: String::new(),
            },
            profile: virtio_resources::fs::microvm::VirtioFsProfile::Microvm {
                stable_id: "fs:microvm0".to_owned(),
                root_identity: attachment.identity.clone(),
                read_only: filesystem_config.access.is_read_only(),
                denied_paths: Vec::new(),
            },
        }
        .into_resource();
        if self.snapshot_destination.is_some() {
            tracing::warn!(
                stable_id = "fs:microvm0",
                access_mode = filesystem_config.access.as_str(),
                "microVM snapshot excludes live host filesystem contents; restore revalidates the external directory and may fail after host changes"
            );
        }
        config.microvm.filesystem = Some(filesystem_config);
        config.microvm.filesystem_bootstrap = true;
        self.resources.filesystem_attachment = Some(attachment);
        config
            .virtio_devices
            .push((openvmm_defs::config::VirtioBus::Mmio, resource));
        Ok(())
    }

    fn add_console(
        &mut self,
        config: &mut Config,
        console: vmservice::VirtioConsoleConfig,
    ) -> anyhow::Result<()> {
        if console.socket_path.is_empty() {
            return Ok(());
        }
        let endpoint_config = if console.connect {
            SerialConfigCli::ConnectPipe(PathBuf::from(&console.socket_path))
        } else {
            SerialConfigCli::Pipe(PathBuf::from(&console.socket_path))
        };
        let (endpoint_config, attachment, snapshot_attachment) =
            crate::microvm::microvm_console_attachment_from_cli(&endpoint_config)?;
        if let Some(snapshot_dir) = &self.snapshot_destination {
            crate::microvm::validate_microvm_console_attachment_namespace(
                &snapshot_attachment,
                snapshot_dir,
            )?;
        }
        let backend = match endpoint_config {
            SerialConfigCli::Pipe(path) => {
                let backend = bind_serial_without_cleanup(&path).with_context(|| {
                    format!("failed to bind virtio console socket: {}", path.display())
                })?;
                self.resources.console_socket_cleanup =
                    crate::microvm::microvm_console_socket_cleanup(path)?;
                backend
            }
            SerialConfigCli::ConnectPipe(path) => {
                crate::serial_io::connect::connect_serial_with_timeout(
                    &path,
                    Duration::from_millis(
                        openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                    ),
                )
                .with_context(|| {
                    format!(
                        "failed to connect virtio console socket: {}",
                        path.display()
                    )
                })?
            }
            _ => unreachable!("path input produced a non-path attachment"),
        };
        self.resources.console_attachment = Some(snapshot_attachment);
        let resource: Resource<VirtioDeviceHandle> =
            virtio_resources::console::VirtioConsoleHandle {
                backend,
                disconnect_policy: VirtioConsoleDisconnectPolicy::Retain,
                attachment: Some(attachment),
            }
            .into_resource();
        config
            .virtio_devices
            .push((openvmm_defs::config::VirtioBus::Mmio, resource));
        Ok(())
    }

    /// Finalizes the fixed virtio-mmio device inventory and discovery command
    /// line.
    pub(super) fn finish_devices(&self, config: &mut Config) -> anyhow::Result<()> {
        if self.filesystem_slot
            && !config
                .virtio_devices
                .iter()
                .any(|(_, device)| device.id() == "virtiofs")
        {
            config.virtio_devices.push((
                openvmm_defs::config::VirtioBus::Mmio,
                virtio_resources::fs::VirtioFsHandle {
                    tag: "microvm".to_owned(),
                    fs: virtio_resources::fs::VirtioFsBackend::Dormant,
                    profile: virtio_resources::fs::microvm::VirtioFsProfile::MicrovmDormant {
                        stable_id: "fs:microvm0".to_owned(),
                    },
                }
                .into_resource(),
            ));
        }

        if self.active && self.authoritative_restore.is_none() {
            let has_console = config
                .virtio_devices
                .iter()
                .any(|(_, device)| device.id() == "virtio-console");
            let LoadMode::Linux {
                cmdline,
                boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
                ..
            } = &mut config.load_mode
            else {
                unreachable!("microVM was validated with direct_boot");
            };
            openvmm_defs::microvm::append_microvm_virtio_discovery(
                cmdline,
                None,
                self.filesystem_slot,
                config.microvm.filesystem.as_ref(),
                has_console,
                config.virtio_devices.iter().any(|(_, device)| {
                    device.id() == openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_ID
                }),
                &config.microvm.sandbox_blocks,
            )?;
        }
        Ok(())
    }

    /// Validates the finished configuration and prepares snapshot RAM state.
    pub(super) fn prepare_launch(
        &mut self,
        config: &Config,
        config_mem_size: u64,
    ) -> anyhow::Result<()> {
        openvmm_defs::microvm::validate_machine_config(config, None)?;
        self.effective_command_line = match &config.load_mode {
            LoadMode::Linux {
                cmdline,
                boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
                ..
            } => Some(cmdline.clone()),
            _ => None,
        };
        self.filesystem.clone_from(&config.microvm.filesystem);
        if let Some(root_path) = self.resources.filesystem_root_path.as_deref() {
            crate::microvm::validate_microvm_filesystem_private_storage(
                root_path,
                self.snapshot_destination.as_deref(),
                self.authoritative_restore
                    .as_ref()
                    .map(|restore| restore.path.as_path()),
                None,
            )?;
        }
        self.snapshot_memory_file = if let Some(destination) = &self.snapshot_destination {
            let parent = destination
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            let parent_metadata = fs_err::symlink_metadata(parent).with_context(|| {
                format!("failed to inspect snapshot parent {}", parent.display())
            })?;
            anyhow::ensure!(
                parent_metadata.file_type().is_dir(),
                "snapshot parent is not a directory: {}",
                parent.display()
            );
            anyhow::ensure!(
                fs_err::symlink_metadata(destination)
                    .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
                "snapshot destination already exists or cannot be inspected: {}",
                destination.display()
            );
            let file = tempfile::Builder::new()
                .prefix(".openvmm-microvm-memory-")
                .tempfile_in(parent)
                .context("failed to create snapshot memory backing")?;
            openvmm_helpers::snapshot::fs::initialize_snapshot_memory_backing_file(
                file.as_file(),
                config_mem_size,
            )
            .context("failed to initialize snapshot memory backing")?;
            Some(file)
        } else {
            None
        };
        self.snapshot_memory_path = self
            .snapshot_memory_file
            .as_ref()
            .map(|file| file.path().to_owned());
        self.snapshot_memory_handle = self
            .snapshot_memory_file
            .as_ref()
            .map(|file| {
                file.reopen()
                    .context("failed to duplicate automatic snapshot RAM handle")
            })
            .transpose()?;
        self.memory_capacity = config.microvm.memory_capacity;
        Ok(())
    }

    /// Takes the microVM-derived VM worker launch fields.
    pub(super) fn worker_fields(&mut self) -> anyhow::Result<WorkerFields> {
        if self.shared_memory.is_none()
            && let Some(file) = &self.snapshot_memory_handle
        {
            let file = file
                .try_clone()
                .context("failed to duplicate snapshot RAM handle for worker")?;
            self.shared_memory = Some(openvmm_helpers::shared_memory::file_to_shared_memory_fd(
                file,
            )?);
        }
        let restore_gate_timeout = (self.restore_online_vp_count.is_some()
            || self
                .authoritative_restore
                .as_ref()
                .is_some_and(|restore| !restore.restore_memory_ranges.is_empty()))
        .then_some(self.restore_gate_timeout);
        Ok(WorkerFields {
            saved_state: self.saved_state.take(),
            shared_memory: self.shared_memory.take(),
            shared_memory_copy_on_write: self.authoritative_restore.is_some(),
            snapshot_restore_guards: self.snapshot_restore_guards.take(),
            snapshot_boundary_requests: self.resources.snapshot_requests.take(),
            snapshot_ready: self.snapshot_ready.take(),
            snapshot_capture_enabled: self.snapshot_destination.is_some(),
            restore_time: self.restore_time.take(),
            restore_gate_timeout,
            restore_vp_count: self.restore_online_vp_count,
        })
    }

    /// Converts the remaining state into VM-controller launch fields.
    pub(super) fn into_controller(self) -> ControllerFields {
        ControllerFields {
            memory_backing_file: self.snapshot_memory_path,
            microvm: MicrovmController {
                active: self.active,
                snapshot_memory_handle: self.snapshot_memory_handle,
                memory_capacity: self.memory_capacity,
                snapshot_requests: self.snapshot_requests,
                snapshot_destination: self.snapshot_destination,
                snapshot_tier: None,
                snapshot_quiesce_timeout: self.snapshot_quiesce_timeout,
                source_hypervisor: self.source_hypervisor,
                effective_command_line: self.effective_command_line,
                resources: self.resources,
                network: None,
                filesystem_slot: self.filesystem_slot,
                filesystem: self.filesystem,
                snapshot_memory_file: self.snapshot_memory_file,
                _private_scratch_dir: None,
            },
        }
    }
}

/// MicroVM-derived values passed to the VM worker.
pub(super) struct WorkerFields {
    pub(super) saved_state: Option<mesh::payload::message::ProtobufMessage>,
    pub(super) shared_memory: Option<SharedMemoryFd>,
    pub(super) shared_memory_copy_on_write: bool,
    pub(super) snapshot_restore_guards: Option<SnapshotRestoreGuards>,
    pub(super) snapshot_boundary_requests: Option<mesh::Receiver<MicrovmSnapshotBoundaryRequest>>,
    pub(super) snapshot_ready: Option<mesh::Sender<MicrovmSnapshotScratchPolicy>>,
    pub(super) snapshot_capture_enabled: bool,
    pub(super) restore_time: Option<RestoreTime>,
    pub(super) restore_gate_timeout: Option<Duration>,
    pub(super) restore_vp_count: Option<u32>,
}

/// MicroVM-derived values passed to the VM controller.
pub(super) struct ControllerFields {
    pub(super) memory_backing_file: Option<PathBuf>,
    pub(super) microvm: MicrovmController,
}

/// Handles controller events emitted by the microVM guest-exit path.
pub(super) fn handle_exit_event(
    event: VmControllerEvent,
    lifecycle: &mut VmLifecycle,
    wait_vm_response: &mut Option<(
        mesh::CancelContext,
        mesh::OneshotSender<Result<(), mesh_rpc::service::Status>>,
    )>,
) -> anyhow::Result<bool> {
    match event {
        VmControllerEvent::ExitRequested { code } => {
            let reason = format!("guest exited with status {code}");
            tracing::info!(code, "guest halted with process status");
            *lifecycle = VmLifecycle::Halted(reason.clone());
            if code == 0 {
                if let Some((_, response)) = wait_vm_response.take() {
                    response.send(Ok(()));
                }
                Ok(true)
            } else {
                if let Some((_, response)) = wait_vm_response.take() {
                    response.send(Err(grpc_error(anyhow::anyhow!(reason.clone()))));
                }
                Err(anyhow::anyhow!(reason))
            }
        }
        VmControllerEvent::ExitFailed { error } => {
            *lifecycle = VmLifecycle::Halted(error.clone());
            if let Some((_, response)) = wait_vm_response.take() {
                response.send(Err(grpc_error(anyhow::anyhow!(error.clone()))));
            }
            Err(anyhow::anyhow!(error))
        }
        _ => unreachable!("only microVM exit events are delegated"),
    }
}

fn openvmm_machine_profile(profile: vmservice::vm_config::MachineProfile) -> MachineProfile {
    match profile {
        vmservice::vm_config::MachineProfile::Standard => MachineProfile::Standard,
        vmservice::vm_config::MachineProfile::Microvm => MachineProfile::Microvm,
    }
}

fn ttrpc_machine_profile(abi_version: u32) -> anyhow::Result<vmservice::vm_config::MachineProfile> {
    anyhow::ensure!(
        abi_version == MICROVM_ABI_VERSION_2,
        "unsupported microVM ABI version {abi_version}; this OpenVMM supports version {MICROVM_ABI_VERSION_2}"
    );
    Ok(vmservice::vm_config::MachineProfile::Microvm)
}

fn validate_restore_console_attachments(
    attachments: &[openvmm_helpers::snapshot::microvm::SnapshotAttachment],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !attachments.iter().any(|attachment| {
            attachment.stable_id == crate::microvm::MICROVM_CONTROL_CONSOLE_STABLE_ID
                || attachment.kind == crate::microvm::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND
        }),
        "OpenVMM management RPC cannot restore control-console snapshots before authenticated broker activation"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use openvmm_defs::microvm::MachineProfile as OpenvmmMachineProfile;
    use test_with_tracing::test;

    #[test]
    fn rpc_service_reports_guest_exit_status() {
        DefaultPool::run_with(async |driver| {
            for (event, expected_error) in [
                (VmControllerEvent::ExitRequested { code: 0 }, None),
                (
                    VmControllerEvent::ExitRequested { code: 1 },
                    Some("guest exited with status 1"),
                ),
                (
                    VmControllerEvent::ExitFailed {
                        error: "console output drain timed out".to_owned(),
                    },
                    Some("console output drain timed out"),
                ),
            ] {
                let directory = tempfile::tempdir().unwrap();
                let listener = UnixListener::bind(directory.path().join("rpc")).unwrap();
                let (events, event_recv) = mesh::channel();
                let (worker_send, worker_recv) = mesh::channel();
                let (response, received) = mesh::oneshot();
                let mut service = VmService {
                    driver: driver.clone(),
                    vm: None,
                    vm_controller: None,
                    vm_controller_events: Some(event_recv),
                    controller_task: None,
                    wait_vm_response: Some((mesh::CancelContext::new(), response)),
                    lifecycle: VmLifecycle::Running,
                    restore_ready_pending: false,
                    rpc_tasks: Vec::new(),
                    transport: ResolvedTransport::Auto,
                    registry: FdRegistry::default(),
                };
                events.send(event);
                let result = mesh::CancelContext::new()
                    .with_timeout(Duration::from_secs(5))
                    .until_cancelled(service.run(listener, worker_recv))
                    .await
                    .expect("guest exit must stop the RPC service");
                drop(worker_send);
                let response = received.await.unwrap();
                if let Some(expected_error) = expected_error {
                    assert!(result.unwrap_err().to_string().contains(expected_error));
                    assert!(response.unwrap_err().message.contains(expected_error));
                } else {
                    result.unwrap();
                    response.unwrap();
                }
                assert!(matches!(service.lifecycle, VmLifecycle::Halted(_)));
            }
        });
    }

    #[test]
    fn ttrpc_standard_virtio_fs_preserves_access_mode() {
        for read_only in [false, true] {
            let handle = build_virtio_fs(vmservice::VirtioFs {
                tag: "share".to_owned(),
                root_path: "host-root".to_owned(),
                read_only,
                ..Default::default()
            })
            .unwrap();
            assert!(matches!(
                handle.profile,
                virtio_resources::fs::microvm::VirtioFsProfile::Standard
            ));
            let virtio_resources::fs::VirtioFsBackend::HostFs {
                root_path,
                mount_options,
            } = handle.fs
            else {
                panic!("standard virtio-fs must use HostFs");
            };
            assert_eq!(root_path, "host-root");
            assert_eq!(mount_options, if read_only { "ro" } else { "" });
        }
    }

    #[test]
    fn ttrpc_standard_virtio_fs_rejects_microvm_mount_fields() {
        for (guest_mount_target, read_write) in [("/share", false), ("", true)] {
            let error = build_virtio_fs(vmservice::VirtioFs {
                tag: "share".to_owned(),
                root_path: "host-root".to_owned(),
                guest_mount_target: guest_mount_target.to_owned(),
                read_write,
                ..Default::default()
            })
            .err()
            .expect("microVM mount fields must be rejected");
            assert!(
                error
                    .to_string()
                    .contains("standard-machine virtio-fs does not accept microVM mount fields")
            );
        }
    }

    #[test]
    fn management_restore_rejects_control_console_attachments_explicitly() {
        let boot = openvmm_helpers::snapshot::microvm::SnapshotAttachment {
            stable_id: crate::microvm::MICROVM_CONSOLE_STABLE_ID.to_owned(),
            kind: crate::microvm::MICROVM_CONSOLE_ATTACHMENT_KIND.to_owned(),
            required: false,
            reconnect_policy: "discard-while-disconnected".to_owned(),
            identity_kind: "disconnected".to_owned(),
            identity: b"discard".to_vec(),
            length: 0,
            reconnect_timeout_ms: 0,
        };
        validate_restore_console_attachments(&[]).unwrap();
        validate_restore_console_attachments(std::slice::from_ref(&boot)).unwrap();

        let mut control = boot.clone();
        control.stable_id = crate::microvm::MICROVM_CONTROL_CONSOLE_STABLE_ID.to_owned();
        control.kind = crate::microvm::MICROVM_CONTROL_CONSOLE_ATTACHMENT_KIND.to_owned();
        for policy in ["discard-while-disconnected", "recreate-listener"] {
            control.reconnect_policy = policy.to_owned();
            let error =
                validate_restore_console_attachments(&[boot.clone(), control.clone()]).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("management RPC cannot restore control-console snapshots")
            );
            assert!(
                error
                    .to_string()
                    .contains("before authenticated broker activation")
            );
        }
    }

    #[test]
    fn ttrpc_microvm_profile_preserves_wire_identity_and_processor_policy() {
        assert_eq!(
            vmservice::vm_config::MachineProfile::Microvm as i32,
            MICROVM_ABI_VERSION_2 as i32
        );
        assert_eq!(
            openvmm_machine_profile(vmservice::vm_config::MachineProfile::Microvm),
            OpenvmmMachineProfile::Microvm
        );
        assert_eq!(
            ttrpc_machine_profile(MICROVM_ABI_VERSION_2).unwrap(),
            vmservice::vm_config::MachineProfile::Microvm
        );
        assert!(vmservice::vm_config::MachineProfile::from_i32(1).is_none());
        assert!(ttrpc_machine_profile(1).is_err());

        for processor_count in [1, 2, 4, 8] {
            assert!(openvmm_defs::microvm::microvm_processor_count_supported(
                processor_count
            ));
        }
        for processor_count in [0, 3, 5, 16] {
            assert!(!openvmm_defs::microvm::microvm_processor_count_supported(
                processor_count
            ));
        }
    }
}
