// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM parts of the VM configuration built from the command line.

use super::MicrovmResources;
use super::MicrovmRestore;
use super::console::ConsoleEndpoint;
use super::console::effective_microvm_console;
use super::console::microvm_console_socket_cleanup;
use super::console::validate_microvm_console_attachment_namespace;
use crate::ConsoleState;
use crate::Options;
use crate::VmResources;
use crate::cli_args::SerialConfigCli;
use crate::cli_args::microvm::MachineProfileCli;
use crate::serial_io;
use anyhow::Context;
use anyhow::bail;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use chipset_resources::microvm::MicrovmSnapshotRequestHandle;
use futures::AsyncReadExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use openvmm_defs::config::Config;
use openvmm_defs::config::LoadMode;
use openvmm_defs::microvm::MachineProfile;
use openvmm_defs::microvm::build_microvm_command_line;
use pal_async::DefaultDriver;
use serial_core::resources::DisconnectedSerialBackendHandle;
use std::cell::RefCell;
use std::thread;
use std::time::Duration;
use virtio_resources::console::attachment::VirtioConsoleDisconnectPolicy;
use virtio_resources::console::attachment::VirtioConsoleReconnectPolicy;
use vm_manifest_builder::MachineArch;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::ResourceId;
use vm_resource::kind::SerialBackendHandle;
use vmgs_resources::VmgsResource;
use vmotherboard::ChipsetDeviceHandle;

/// MicroVM state computed while building a VM configuration from the command
/// line. All methods are no-ops for the standard machine profile.
pub(crate) struct MicrovmConfigBuilder<'a> {
    opt: &'a Options,
    restore: &'a MicrovmRestore,
    active: bool,
    console: Option<ConsoleEndpoint>,
    portb: Option<Resource<SerialBackendHandle>>,
    resources: MicrovmResources,
}

impl<'a> MicrovmConfigBuilder<'a> {
    /// Validates the microVM options.
    pub(crate) fn new(opt: &'a Options, restore: &'a MicrovmRestore) -> anyhow::Result<Self> {
        let active = opt.machine == MachineProfileCli::Microvm;
        let restore_machine_contract = restore.machine_contract.as_ref();
        if let Some(contract) = restore_machine_contract {
            openvmm_helpers::snapshot::microvm::validate_supported_microvm_contract(contract)?;
        }
        opt.validate_microvm_options()?;

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
        if let Some((_, _, attachment)) = &console
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
            ..Default::default()
        };

        Ok(Self {
            opt,
            restore,
            active,
            console,
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
        let backend = if self
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

    /// Connects the host side of the boot virtio-console, returning its
    /// backend.
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
                SerialConfigCli::Console => Some(setup_host_console(
                    "virtio-console",
                    SerialConfigCli::Console,
                    "hvc1",
                    console_state,
                    serial_driver,
                )?),
                SerialConfigCli::None => Some(DisconnectedSerialBackendHandle.into_resource()),
                _ => unreachable!("microVM console backend was validated as an attachment"),
            }
        } else {
            None
        };
        Ok(virtio_console_backend)
    }

    /// Adds the portb, shutdown, and snapshot-request chipset devices.
    pub(crate) fn add_chipset_devices(
        &mut self,
        chipset_devices: &mut Vec<ChipsetDeviceHandle>,
    ) -> anyhow::Result<()> {
        let Some(io) = self.portb.take() else {
            return Ok(());
        };
        let opt = self.opt;
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmPortbHandle::ID.to_owned(),
            resource: MicrovmPortbHandle { io }.into_resource(),
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
            openvmm_defs::microvm::append_microvm_virtio_discovery(cmdline, has_console)?;
        }
        openvmm_defs::microvm::validate_machine_config(cfg, requested_hypervisor)?;

        resources.microvm = std::mem::take(&mut self.resources);
        Ok(())
    }
}

/// Connects a portb or boot virtio-console endpoint to the host console or
/// stderr, returning its backend.
fn setup_host_console(
    name: &str,
    backend: SerialConfigCli,
    device: &'static str,
    console_state: &RefCell<Option<ConsoleState<'static>>>,
    serial_driver: &DefaultDriver,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
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
            spawn_output(name, serial_read, term::raw_stdout())?;
            config
        }
        SerialConfigCli::Stderr => {
            let (config, serial) = serial_io::anonymous_serial_pair(serial_driver)?;
            spawn_output(name, serial, term::raw_stderr())?;
            config
        }
        _ => unreachable!("microVM host consoles use the console or stderr"),
    })
}

/// Relays `input` to `output` on a dedicated thread.
fn spawn_output(
    name: &str,
    input: impl futures::AsyncRead + Send + Unpin + 'static,
    output: impl std::io::Write + Send + 'static,
) -> anyhow::Result<()> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = block_on(futures::io::copy(input, &mut AllowStdIo::new(output)));
        })
        .with_context(|| format!("failed to spawn the {name} output relay"))?;
    Ok(())
}

fn build_effective_microvm_command_line(
    user_args: &[String],
    processor_count: u32,
    has_console: bool,
) -> anyhow::Result<String> {
    let mut cmdline = build_microvm_command_line(user_args, has_console)?;
    openvmm_defs::microvm::append_microvm_processor_limit(&mut cmdline, processor_count)?;
    Ok(cmdline)
}
