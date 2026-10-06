// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM parts of the VM configuration built from the command line.

use super::MicrovmResources;
use super::MicrovmRestore;
use super::console::ConsoleEndpoint;
use super::console::effective_microvm_console;
use super::console::effective_microvm_control_console;
use super::console::microvm_console_attachments_share_endpoint;
use super::console::microvm_console_socket_cleanup;
use super::console::microvm_control_broker_config;
use super::console::validate_microvm_console_attachment_namespace;
use super::filesystem::EffectiveMicrovmFilesystem;
use super::filesystem::effective_microvm_filesystems;
use super::filesystem::microvm_filesystem_slot_from_snapshot;
use super::network::EffectiveMicrovmNetwork;
use super::network::effective_microvm_network;
use super::network::microvm_network_endpoint;
use super::output::MicrovmOutputDrain;
use super::output::OutputCompletion;
use super::output::spawn_output;
use super::restore::fresh_microvm_generation_id;
use crate::ConsoleState;
use crate::Options;
use crate::VmResources;
use crate::cli_args;
use crate::cli_args::SerialConfigCli;
use crate::cli_args::VirtioBusCli;
use crate::cli_args::microvm::MachineProfileCli;
use crate::serial_io;
use crate::storage_builder::StorageBuilder;
use anyhow::Context;
use anyhow::bail;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use futures::AsyncReadExt;
use net_backend_resources::consomme::static_ipv4::StaticIpv4Config;
use openvmm_defs::config::Config;
use openvmm_defs::config::DeviceVtl;
use openvmm_defs::config::LoadMode;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::build_microvm_command_line;
use openvmm_defs::microvm::build_microvm_control_command_line;
use pal_async::DefaultDriver;
use serial_core::resources::DisconnectedSerialBackendHandle;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;
use virtio_resources::console::control::VirtioControlConsoleBrokerConfig;
use vm_manifest_builder::MachineArch;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vm_resource::kind::VirtioDeviceHandle;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;

/// MicroVM state computed while building a VM configuration from the command
/// line. All methods are no-ops for the standard machine profile.
pub(crate) struct MicrovmConfigBuilder<'a> {
    opt: &'a Options,
    restore: &'a MicrovmRestore,
    active: bool,
    network: Option<EffectiveMicrovmNetwork>,
    filesystem_slot: bool,
    filesystems: Vec<EffectiveMicrovmFilesystem>,
    gateway_dns: bool,
    console: Option<ConsoleEndpoint>,
    control_console: Option<ConsoleEndpoint>,
    control_broker_config: Option<VirtioControlConsoleBrokerConfig>,
    control_console_backend: Option<Resource<SerialBackendHandle>>,
    portb: Option<(Resource<SerialBackendHandle>, OutputCompletion)>,
    resources: MicrovmResources,
}

impl<'a> MicrovmConfigBuilder<'a> {
    /// Validates the microVM options and resolves the effective network,
    /// filesystem, and console attachments.
    pub(crate) fn new(opt: &'a Options, restore: &'a MicrovmRestore) -> anyhow::Result<Self> {
        let active = opt.machine == MachineProfileCli::Microvm;
        let restore_machine_contract = restore.machine_contract.as_ref();
        if let Some(contract) = restore_machine_contract {
            openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(contract)?;
        }
        opt.validate_microvm_options()?;
        let network = if active {
            effective_microvm_network(opt, restore_machine_contract)?
        } else {
            None
        };
        let filesystem_slot = if active {
            restore_machine_contract
                .map(microvm_filesystem_slot_from_snapshot)
                .transpose()?
                .unwrap_or(true)
        } else {
            false
        };
        let filesystems = if active {
            effective_microvm_filesystems(
                &opt.microvm.microvm_mount,
                &opt.microvm.microvm_mount_deny,
                opt.microvm.microvm_mount_owner.map_or(
                    openvmm_defs::microvm::MicrovmFilesystemOwner::Vmm,
                    Into::into,
                ),
                restore_machine_contract,
            )?
        } else {
            Vec::new()
        };
        if opt.microvm.snapshot_destination.is_some() {
            for filesystem in &filesystems {
                tracing::warn!(
                    stable_id = filesystem.attachment.stable_id,
                    access_mode = filesystem.config.access.as_str(),
                    "microVM snapshot excludes live host filesystem contents; restore revalidates the external directory and may fail after host changes"
                );
            }
        }
        let gateway_dns = network
            .as_ref()
            .is_some_and(|network| network.policy.allows_gateway_dns());

        if active
            && (opt.com1.is_some()
                || opt.com2.is_some()
                || opt.com3.is_some()
                || opt.com4.is_some()
                || opt.vmbus_com1_serial.is_some()
                || opt.vmbus_com2_serial.is_some()
                || opt.debugcon.is_some())
        {
            bail!("microVM does not expose UART, debugcon, or VMBus serial");
        }

        let console = if active {
            effective_microvm_console(opt.virtio_console.as_ref(), restore_machine_contract)?
        } else {
            None
        };
        let control_console = if active {
            effective_microvm_control_console(
                opt.microvm.microvm_control_console.as_ref(),
                restore_machine_contract,
            )?
        } else {
            None
        };
        opt.validate_control_stdin_console(console.as_ref().map(|(config, _, _)| config))?;
        let control_broker_config = control_console
            .as_ref()
            .map(|(endpoint, _, _)| microvm_control_broker_config(opt, endpoint))
            .transpose()?;
        anyhow::ensure!(
            control_console.is_none() || console.is_some(),
            "microVM control console requires the boot virtio-console"
        );
        if let (Some((_, _, boot)), Some((_, _, control))) = (&console, &control_console)
            && control.identity_kind != "disconnected"
        {
            anyhow::ensure!(
                !microvm_console_attachments_share_endpoint(boot, control),
                "microVM boot and control consoles require distinct endpoints"
            );
        }
        if let Some((_, _, attachment)) = &console
            && let Some(snapshot_dir) = opt
                .restore_snapshot
                .as_deref()
                .or(opt.microvm.snapshot_destination.as_deref())
        {
            validate_microvm_console_attachment_namespace(attachment, snapshot_dir)?;
        }
        if let Some((_, _, attachment)) = &control_console
            && let Some(snapshot_dir) = opt
                .restore_snapshot
                .as_deref()
                .or(opt.microvm.snapshot_destination.as_deref())
        {
            validate_microvm_console_attachment_namespace(attachment, snapshot_dir)?;
        }

        let resources = MicrovmResources {
            console_attachment: console
                .as_ref()
                .map(|(_, _, attachment)| attachment.clone()),
            control_console_attachment: control_console
                .as_ref()
                .map(|(_, _, attachment)| attachment.clone()),
            network_attachment: network.as_ref().map(|network| network.attachment.clone()),
            egress_policy: network.as_ref().map(|network| network.policy.clone()),
            filesystem_attachments: filesystems
                .iter()
                .map(|filesystem| filesystem.attachment.clone())
                .collect(),
            filesystem_root_paths: filesystems
                .iter()
                .map(|filesystem| PathBuf::from(&filesystem.root_path))
                .collect(),
            ..Default::default()
        };

        Ok(Self {
            opt,
            restore,
            active,
            network,
            filesystem_slot,
            filesystems,
            gateway_dns,
            console,
            control_console,
            control_broker_config,
            control_console_backend: None,
            portb: None,
            resources,
        })
    }

    /// Returns whether the microVM machine profile is selected.
    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Returns the backend used for `--com1` when it is not specified.
    pub(crate) fn default_com1_backend(&self) -> SerialConfigCli {
        if self.active {
            SerialConfigCli::None
        } else {
            SerialConfigCli::Console
        }
    }

    /// Connects the host side of the portb console (`hvc0`).
    ///
    /// This must run before the other serial devices so that portb owns the
    /// interactive console when the boot virtio-console does not.
    pub(crate) fn setup_portb(
        &mut self,
        console_state: &RefCell<Option<ConsoleState<'static>>>,
        serial_driver: &DefaultDriver,
    ) -> anyhow::Result<()> {
        if !self.active {
            return Ok(());
        }
        let backend = if self.opt.microvm.microvm_control_auth_stdin
            || self
                .console
                .as_ref()
                .is_some_and(|(config, _, _)| matches!(config, SerialConfigCli::Console))
        {
            SerialConfigCli::Stderr
        } else {
            SerialConfigCli::Console
        };
        self.portb = Some(setup_host_console(
            "microvm-portb",
            backend,
            "hvc0",
            console_state,
            serial_driver,
        )?);
        Ok(())
    }

    /// Connects the host side of the boot virtio-console and the dedicated
    /// control console, returning the boot console backend.
    pub(crate) fn setup_virtio_consoles(
        &mut self,
        console_state: &RefCell<Option<ConsoleState<'static>>>,
        serial_driver: &DefaultDriver,
    ) -> anyhow::Result<Option<Resource<SerialBackendHandle>>> {
        let virtio_console_backend = if let Some(serial_cfg) =
            self.console.as_ref().map(|(config, _, _)| config.clone())
        {
            match serial_cfg {
                SerialConfigCli::Pipe(path) => {
                    let backend = serial_io::connect::bind_serial_without_cleanup(&path)
                        .with_context(|| {
                            format!(
                                "failed to bind microVM virtio console listener {}",
                                path.display()
                            )
                        })?;
                    self.resources.console_socket_cleanup = microvm_console_socket_cleanup(path)?;
                    Some(backend)
                }
                SerialConfigCli::Tcp(address) => Some(serial_io::bind_tcp_serial(&address)?),
                SerialConfigCli::ConnectPipe(path) => Some(
                    serial_io::connect::connect_serial_with_timeout(
                        &path,
                        Duration::from_millis(
                            openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                        ),
                    )
                    .with_context(|| {
                        format!(
                            "failed to reconnect microVM virtio console client {}",
                            path.display()
                        )
                    })?,
                ),
                SerialConfigCli::ConnectTcp(address) => {
                    Some(serial_io::connect::connect_tcp_serial(
                        &address,
                        Duration::from_millis(
                            openvmm_defs::microvm::MICROVM_CONSOLE_RECONNECT_TIMEOUT_MS,
                        ),
                    )?)
                }
                SerialConfigCli::Console => {
                    let (backend, _) = setup_host_console(
                        "virtio-console",
                        SerialConfigCli::Console,
                        "hvc1",
                        console_state,
                        serial_driver,
                    )?;
                    Some(backend)
                }
                SerialConfigCli::None => Some(DisconnectedSerialBackendHandle.into_resource()),
                _ => unreachable!("microVM console backend was validated as an attachment"),
            }
        } else {
            None
        };
        if let Some(serial_cfg) = self
            .control_console
            .as_ref()
            .map(|(config, _, _)| config.clone())
        {
            self.control_console_backend = match serial_cfg {
                SerialConfigCli::Pipe(path) => {
                    let backend =
                        serial_io::microvm::bind_control_serial(&path).with_context(|| {
                            format!(
                                "failed to bind microVM control console listener {}",
                                path.display()
                            )
                        })?;
                    self.resources.control_console_socket_cleanup =
                        microvm_console_socket_cleanup(path)?;
                    Some(backend)
                }
                SerialConfigCli::None => Some(DisconnectedSerialBackendHandle.into_resource()),
                _ => unreachable!("microVM control console backend was validated"),
            };
        }
        Ok(virtio_console_backend)
    }

    /// Adds the fixed-role sandbox block devices.
    pub(crate) async fn add_sandbox_blocks(
        &self,
        storage: &mut StorageBuilder,
    ) -> anyhow::Result<()> {
        let opt = self.opt;
        for block in &opt.microvm.microvm_sandbox_block {
            let disk = &block.disk;
            anyhow::ensure!(
                disk.vtl == DeviceVtl::Vtl0
                    && !disk.is_dvd
                    && disk.underhill.is_none()
                    && disk.pcie_port.is_none()
                    && disk.controller.is_none()
                    && disk.nsid.is_none()
                    && disk.lun.is_none()
                    && disk.relay.is_none(),
                "--microvm-sandbox-block accepts only a plain VTL0 disk backend"
            );
            storage
                .add_microvm_sandbox_block(
                    block.role,
                    &disk.kind,
                    disk.read_only,
                    opt.microvm.snapshot_destination.is_some() || opt.restore_snapshot.is_some(),
                )
                .await?;
        }
        Ok(())
    }

    /// Adds the portb, shutdown, and snapshot-request chipset devices.
    pub(crate) fn add_chipset_devices(
        &mut self,
        chipset_devices: &mut Vec<ChipsetDeviceHandle>,
    ) -> anyhow::Result<()> {
        let Some((io, output_completion)) = self.portb.take() else {
            return Ok(());
        };
        let opt = self.opt;
        let (drain, output_drain) = MicrovmOutputDrain::new(Some(output_completion));
        self.resources.output_drain = Some(drain);
        let (generation_id, time_abi_restore) = if self.restore.machine_contract.is_some() {
            let (generation_id, base) = self
                .restore
                .time_abi_restore_packet(opt.microvm.restore_processors)?;
            (
                generation_id,
                Some(self.resources.time_abi_restore_packet(base)),
            )
        } else {
            (fresh_microvm_generation_id()?, None)
        };
        let hooks = time_abi_test_hooks(opt)?;
        let time_abi = super::time_abi_chipset(chipset_devices, &hooks, time_abi_restore);
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmPortbHandle::ID.to_owned(),
            resource: MicrovmPortbHandle {
                io,
                generation_id,
                output_drain: Some(output_drain),
                time_abi,
            }
            .into_resource(),
        });
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmShutdownHandle::ID.to_owned(),
            resource: MicrovmShutdownHandle.into_resource(),
        });
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmSnapshotRequestHandle::ID.to_owned(),
            resource: {
                let (notify, requests) = mesh::channel();
                self.resources.snapshot_requests = Some(requests);
                MicrovmSnapshotRequestHandle {
                    notify: Some(notify),
                    input_gate_timeout: Duration::from_millis(
                        opt.microvm.snapshot_quiesce_timeout_ms,
                    ),
                }
                .into_resource()
            },
        });
        Ok(())
    }

    /// Returns the MP-table Linux load mode of the microVM profile.
    pub(crate) fn load_mode(&self, arch: MachineArch) -> anyhow::Result<Option<LoadMode>> {
        if !self.active {
            return Ok(None);
        }
        let opt = self.opt;
        if arch != MachineArch::X86_64 {
            bail!("the microVM profile requires an x86-64 guest");
        }
        if opt.igvm.is_some() || opt.pcat || opt.uefi.is_some() {
            bail!("the microVM profile requires Linux direct boot");
        }

        let (kernel, initrd, cmdline) = if let Some(contract) = &self.restore.machine_contract {
            (
                tempfile::tempfile().context("failed to create inert restore kernel handle")?,
                None,
                contract.effective_command_line.clone(),
            )
        } else {
            let kernel = fs_err::File::open(
                (opt.kernel.0)
                    .as_ref()
                    .context("must provide a Linux kernel when using --machine microvm")?,
            )
            .context("failed to open Linux kernel")?;
            let initrd = (opt.initrd.0)
                .as_ref()
                .map(fs_err::File::open)
                .transpose()
                .context("failed to open Linux initrd")?;
            (
                kernel.into(),
                initrd.map(Into::into),
                build_effective_microvm_command_line(
                    &opt.cmdline,
                    opt.processors,
                    self.console.is_some(),
                    self.control_console.is_some(),
                    opt.microvm.microvm_workload_identity,
                    opt.microvm.microvm_lifecycle,
                )?,
            )
        };

        Ok(Some(LoadMode::Linux {
            kernel,
            initrd,
            cmdline,
            enable_serial: false,
            isolation: openvmm_defs::config::LinuxIsolationConfig::None,
            boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
            smbios: Box::default(),
        }))
    }

    /// Rejects VMGS before the standard VMGS resource is opened.
    pub(crate) fn validate_vmgs(&self) -> anyhow::Result<()> {
        if self.active && self.opt.vmgs.is_some() {
            bail!("microVM does not support VMGS");
        }
        Ok(())
    }

    /// Drops the default VMGS resource, which the microVM does not expose.
    pub(crate) fn filter_vmgs(&self, vmgs: &mut Option<VmgsResource>) {
        if self.active {
            *vmgs = None;
        }
    }

    /// Adds the fixed-slot virtio-net and virtio-fs devices.
    pub(crate) fn add_virtio_devices(
        &mut self,
        add_virtio_device: &mut impl FnMut(VirtioBusCli, Resource<VirtioDeviceHandle>),
        resources: &mut VmResources,
    ) -> anyhow::Result<()> {
        if let Some(network) = self.network.as_ref() {
            let config = &network.config;
            let endpoint = microvm_network_endpoint(
                config,
                &network.policy,
                &self.opt.microvm.host_loopback_forward,
                resources,
            )?;
            add_virtio_device(
                VirtioBusCli::Mmio,
                virtio_resources::net::VirtioNetHandle {
                    max_queues: Some(1),
                    mac_address: config.guest_mac,
                    endpoint,
                    egress_policy: Some(network.policy.clone()),
                    save_restore: true,
                    static_ipv4: Some(StaticIpv4Config {
                        guest_ipv4: config.guest_ipv4,
                        prefix_length: config.prefix_length,
                        gateway_ipv4: config.derived_gateway_ipv4,
                        gateway_mac: config.gateway_mac,
                    }),
                    effective_features: Some(openvmm_defs::microvm::MICROVM_VIRTIO_NET_FEATURES),
                }
                .into_resource(),
            );
        }

        let slot_count = openvmm_defs::microvm::microvm_filesystem_slot_count(
            self.filesystem_slot,
            self.filesystems.len(),
        )?;
        for (index, slot) in openvmm_defs::microvm::MICROVM_FILESYSTEM_SLOTS
            .iter()
            .take(slot_count)
            .enumerate()
        {
            let (fs, profile) = if let Some(filesystem) = self.filesystems.get(index) {
                (
                    virtio_resources::fs::VirtioFsBackend::HostFs {
                        root_path: filesystem.root_path.clone(),
                        mount_options: String::new(),
                    },
                    virtio_resources::fs::microvm::VirtioFsProfile::Microvm {
                        stable_id: slot.stable_id.to_owned(),
                        root_identity: filesystem.attachment.identity.clone(),
                        read_only: filesystem.config.access.is_read_only(),
                        denied_paths: filesystem.config.denied_paths.clone(),
                        caller_identity: filesystem.config.owner.is_caller(),
                    },
                )
            } else {
                (
                    virtio_resources::fs::VirtioFsBackend::Dormant,
                    virtio_resources::fs::microvm::VirtioFsProfile::MicrovmDormant {
                        stable_id: slot.stable_id.to_owned(),
                    },
                )
            };
            add_virtio_device(
                VirtioBusCli::Mmio,
                virtio_resources::fs::VirtioFsHandle {
                    tag: slot.tag.to_owned(),
                    fs,
                    profile,
                }
                .into_resource(),
            );
        }
        Ok(())
    }

    /// Builds the boot virtio-console device handle.
    pub(crate) fn virtio_console_handle(
        &self,
        backend: Resource<SerialBackendHandle>,
    ) -> virtio_resources::console::VirtioConsoleHandle {
        virtio_resources::console::VirtioConsoleHandle {
            backend,
            disconnect_policy: if self.active
                && !self.console.as_ref().is_some_and(|(_, attachment, _)| {
                    attachment.reconnect_policy
                        == VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
                }) {
                VirtioConsoleDisconnectPolicy::Retain
            } else {
                VirtioConsoleDisconnectPolicy::Discard
            },
            attachment: self
                .console
                .as_ref()
                .map(|(_, attachment, _)| attachment.clone()),
        }
    }

    /// Adds the dedicated control virtio-console device.
    pub(crate) fn add_control_console(
        &mut self,
        add_virtio_device: &mut impl FnMut(VirtioBusCli, Resource<VirtioDeviceHandle>),
    ) -> anyhow::Result<()> {
        if let Some(backend) = self.control_console_backend.take() {
            let broker_config = self.control_broker_config.take().context(
                "control-console backend is missing broker authentication configuration",
            )?;
            let resource: Resource<VirtioDeviceHandle> =
                virtio_resources::console::control::VirtioControlConsoleHandle {
                    backend,
                    broker_config,
                    attachment: self
                        .control_console
                        .as_ref()
                        .map(|(_, attachment, _)| attachment.clone()),
                }
                .into_resource();
            add_virtio_device(VirtioBusCli::Mmio, resource);
        }
        Ok(())
    }

    /// Completes the microVM configuration after the storage devices have been
    /// added, and validates the machine contract of every profile.
    pub(crate) fn finish(
        mut self,
        cfg: &mut Config,
        resources: &mut VmResources,
    ) -> anyhow::Result<()> {
        let opt = self.opt;
        let restore_machine_contract = self.restore.machine_contract.as_ref();
        if self.active {
            cfg.processor_topology.vps_per_socket = Some(opt.processors);
            cfg.processor_topology.enable_smt = Some(false);
            #[cfg(guest_arch = "x86_64")]
            {
                cfg.processor_topology.arch = Some(openvmm_defs::config::ArchTopologyConfig::X86(
                    openvmm_defs::config::X86TopologyConfig {
                        apic_id_offset: 0,
                        x2apic: openvmm_defs::config::X2ApicConfig::Unsupported,
                    },
                ));
            }
        }
        cfg.microvm.network = self.network.as_ref().map(|network| network.config.clone());
        let microvm_filesystems = self
            .filesystems
            .iter()
            .map(|filesystem| filesystem.config.clone())
            .collect::<Vec<_>>();
        cfg.microvm.filesystem_bootstrap = restore_machine_contract
            .map(|contract| contract.microvm_filesystem.is_some())
            .unwrap_or_else(|| !microvm_filesystems.is_empty());
        cfg.microvm.filesystems = microvm_filesystems;
        cfg.microvm.memory_capacity = restore_machine_contract
            .and_then(|contract| {
                (contract.memory_expansion_version != 0).then_some(contract.memory_capacity_bytes)
            })
            .or(opt.microvm.memory_capacity.map(|capacity| capacity.0));
        cfg.microvm.snapshot_memory_ranges = restore_machine_contract
            .filter(|contract| contract.memory_expansion_version != 0)
            .map(|contract| {
                contract
                    .memory_ranges
                    .iter()
                    .map(|range| {
                        memory_range::MemoryRange::new(
                            range.gpa_start..range.gpa_start + range.length,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        cfg.microvm.restore_memory_ranges = self
            .restore
            .memory_ranges
            .iter()
            .map(|range| {
                memory_range::MemoryRange::new(range.gpa_start..range.gpa_start + range.length)
            })
            .collect();
        if cfg.machine_profile == MachineProfile::Microvm {
            let hooks = time_abi_test_hooks(opt)?;
            if hooks.active() {
                tracing::warn!(?hooks, "time ABI test hooks are active");
            }
            let requested_profile = opt.microvm.cpu_profile.as_deref().unwrap_or("auto");
            let (cpu_profile, generation) = match self.restore.time_abi_records() {
                // The controller's restore preflight checks the requested
                // profile against the snapshot's.
                Some((time, record)) => {
                    (record.id.clone(), time.capture_generation.saturating_add(1))
                }
                None => (requested_profile.to_owned(), 0),
            };
            cfg.microvm.time_abi = Some(openvmm_defs::time_abi::TimeAbiParameters {
                cpu_profile,
                generation,
                hooks,
            });
        } else {
            anyhow::ensure!(
                opt.microvm.cpu_profile.is_none()
                    && opt.microvm.x_time_abi_test_hook.is_empty()
                    && !opt.microvm.x_time_abi_verify,
                "--cpu-profile, --x-time-abi-test-hook, and --x-time-abi-verify require the microVM machine profile"
            );
        }

        let requested_hypervisor = opt
            .hypervisor
            .as_deref()
            .and_then(|spec| spec.split(':').next());
        if cfg.machine_profile == MachineProfile::Microvm && restore_machine_contract.is_none() {
            let cmdline = match &mut cfg.load_mode {
                LoadMode::Linux {
                    cmdline,
                    boot_mode: openvmm_defs::config::LinuxDirectBootMode::MpTable,
                    ..
                } => cmdline,
                _ => {
                    unreachable!("microVM configuration was constructed with a supported load mode")
                }
            };
            let has_console = cfg
                .virtio_devices
                .iter()
                .any(|(_, device)| device.id() == "virtio-console");
            let has_control_console = cfg.virtio_devices.iter().any(|(_, device)| {
                device.id() == openvmm_defs::microvm::MICROVM_VIRTIO_CONTROL_CONSOLE_ID
            });
            let network_irq = cfg
                .microvm
                .network
                .as_ref()
                .map(|_| openvmm_defs::microvm::microvm_virtio_net_irq(requested_hypervisor))
                .transpose()?;
            let network = cfg
                .microvm
                .network
                .as_ref()
                .zip(network_irq)
                .map(|(network, irq)| (network, irq, self.gateway_dns));
            if let Some(snapshot_tier) = opt.microvm.snapshot_tier {
                anyhow::ensure!(
                    !cmdline
                        .split_ascii_whitespace()
                        .any(|token| token.starts_with("nvx_snapshot_tier=")),
                    "nvx_snapshot_tier is reserved for the host snapshot policy"
                );
                if !cmdline.is_empty() {
                    cmdline.push(' ');
                }
                cmdline.push_str("nvx_snapshot_tier=");
                cmdline.push_str(snapshot_tier.manifest_name());
            }
            openvmm_defs::microvm::append_microvm_virtio_discovery(
                cmdline,
                network,
                self.filesystem_slot,
                &cfg.microvm.filesystems,
                has_console,
                has_control_console,
                &cfg.microvm.sandbox_blocks,
            )?;
        }
        openvmm_defs::microvm::validate_machine_config(cfg, requested_hypervisor)?;

        let sandbox_block_sources = std::mem::take(&mut resources.microvm.sandbox_block_sources);
        resources.microvm = MicrovmResources {
            sandbox_block_sources,
            ..std::mem::take(&mut self.resources)
        };
        Ok(())
    }
}

/// Connects a portb or boot virtio-console endpoint to the host console or
/// stderr, returning its backend and output-relay completion.
fn setup_host_console(
    name: &str,
    backend: SerialConfigCli,
    device: &'static str,
    console_state: &RefCell<Option<ConsoleState<'static>>>,
    serial_driver: &DefaultDriver,
) -> anyhow::Result<(Resource<SerialBackendHandle>, OutputCompletion)> {
    Ok(match backend {
        SerialConfigCli::Console => {
            if let Some(console_state) = console_state.borrow().as_ref() {
                bail!("console already set by {}", console_state.device);
            }
            let (config, serial) = serial_io::anonymous_serial_pair(serial_driver)?;
            let (serial_read, serial_write) = AsyncReadExt::split(serial);
            *console_state.borrow_mut() = Some(ConsoleState {
                device,
                input: Box::new(serial_write),
            });
            let completed = spawn_output(name, serial_read, term::raw_stdout())?;
            (config, completed)
        }
        SerialConfigCli::Stderr => {
            let (config, serial) = serial_io::anonymous_serial_pair(serial_driver)?;
            let completed = spawn_output(name, serial, term::raw_stderr())?;
            (config, completed)
        }
        _ => unreachable!("microVM host consoles use the console or stderr"),
    })
}

/// Returns the time ABI test hooks selected on the command line.
fn time_abi_test_hooks(opt: &Options) -> anyhow::Result<virt::time_abi::TimeAbiTestHooks> {
    Ok(virt::time_abi::TimeAbiTestHooks::parse(
        &opt.microvm.x_time_abi_test_hook,
    )?)
}

fn build_effective_microvm_command_line(
    user_args: &[String],
    processor_count: u32,
    has_console: bool,
    has_control_console: bool,
    workload_identity: Option<cli_args::microvm::MicrovmWorkloadIdentityCli>,
    lifecycle: Option<cli_args::microvm::MicrovmLifecycleCli>,
) -> anyhow::Result<String> {
    let mut cmdline = if has_control_console {
        build_microvm_control_command_line(user_args, has_console)
    } else {
        build_microvm_command_line(user_args, has_console)
    }?;
    openvmm_defs::microvm::append_microvm_processor_limit(&mut cmdline, processor_count)?;
    if let Some(identity) = workload_identity {
        openvmm_defs::microvm::append_microvm_workload_identity(
            &mut cmdline,
            identity.uid,
            identity.gid,
        )?;
    }
    if let Some(lifecycle) = lifecycle {
        openvmm_defs::microvm::append_microvm_lifecycle(
            &mut cmdline,
            lifecycle == cli_args::microvm::MicrovmLifecycleCli::Managed,
        )?;
    }
    Ok(cmdline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    #[test]
    fn boot_only_command_line_preserves_control_free_arguments() {
        let user_args = [
            r#"note="left right""#.to_owned(),
            "--".to_owned(),
            "driver_async_probe=virtio_mmio".to_owned(),
            "nvx_control_tty=hvc9".to_owned(),
            "virtio-mmio.device=0x1000@0xc0000000:1".to_owned(),
        ];
        assert!(
            build_effective_microvm_command_line(&user_args, 1, true, false, None, None).is_ok()
        );
        assert!(
            build_effective_microvm_command_line(&user_args, 1, true, true, None, None).is_err()
        );
    }

    #[test]
    fn workload_identity_is_host_owned_and_fixed() {
        let identity = cli_args::microvm::MicrovmWorkloadIdentityCli {
            uid: 65_534,
            gid: 65_534,
        };
        let command_line =
            build_effective_microvm_command_line(&[], 1, false, false, Some(identity), None)
                .unwrap();
        assert!(command_line.contains("nvx_workload_uid=65534"));
        assert!(command_line.contains("nvx_workload_gid=65534"));
        assert!(
            build_effective_microvm_command_line(
                &["nvx_workload_uid=1".to_owned()],
                1,
                false,
                false,
                Some(identity),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn workload_lifecycle_is_host_owned() {
        let command_line = build_effective_microvm_command_line(
            &[],
            1,
            true,
            true,
            Some(cli_args::microvm::MicrovmWorkloadIdentityCli {
                uid: 65_534,
                gid: 65_534,
            }),
            Some(cli_args::microvm::MicrovmLifecycleCli::Managed),
        )
        .unwrap();
        assert!(command_line.contains("nvx_lifecycle=managed"));
        assert!(
            build_effective_microvm_command_line(
                &["nvx_lifecycle=one-shot".to_owned()],
                1,
                true,
                true,
                None,
                Some(cli_args::microvm::MicrovmLifecycleCli::Managed),
            )
            .is_err()
        );
    }
}
