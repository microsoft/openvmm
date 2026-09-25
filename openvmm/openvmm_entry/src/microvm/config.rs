// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MicroVM parts of the VM configuration built from the command line.

use crate::ConsoleState;
use crate::Options;
use crate::cli_args::SerialConfigCli;
use crate::cli_args::microvm::MachineProfileCli;
use crate::serial_io;
use anyhow::Context;
use anyhow::bail;
use chipset_resources::microvm::MicrovmPortbHandle;
use chipset_resources::microvm::MicrovmShutdownHandle;
use futures::AsyncReadExt;
use futures::executor::block_on;
use futures::io::AllowStdIo;
use openvmm_defs::config::Config;
use openvmm_defs::config::LoadMode;
use openvmm_defs::microvm::build_microvm_command_line;
use pal_async::DefaultDriver;
use std::cell::RefCell;
use std::thread;
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
    active: bool,
    portb: Option<Resource<SerialBackendHandle>>,
}

impl<'a> MicrovmConfigBuilder<'a> {
    /// Validates the microVM options.
    pub(crate) fn new(opt: &'a Options) -> anyhow::Result<Self> {
        let active = opt.machine == MachineProfileCli::Microvm;
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

        Ok(Self {
            opt,
            active,
            portb: None,
        })
    }

    /// Returns the backend used for `--com1` when it is not specified.
    pub(crate) fn default_com1_backend(&self) -> SerialConfigCli {
        if self.active {
            SerialConfigCli::None
        } else {
            SerialConfigCli::Console
        }
    }

    /// Connects the host side of the portb console (`hvc0`) to the host
    /// console.
    ///
    /// This must run before the other serial devices so that portb owns the
    /// interactive console.
    pub(crate) fn setup_portb(
        &mut self,
        console_state: &RefCell<Option<ConsoleState<'static>>>,
        serial_driver: &DefaultDriver,
    ) -> anyhow::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.portb = Some(setup_host_console(
            "microvm-portb",
            "hvc0",
            console_state,
            serial_driver,
        )?);
        Ok(())
    }

    /// Adds the portb and shutdown chipset devices.
    pub(crate) fn add_chipset_devices(
        &mut self,
        chipset_devices: &mut Vec<ChipsetDeviceHandle>,
    ) -> anyhow::Result<()> {
        let Some(io) = self.portb.take() else {
            return Ok(());
        };
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmPortbHandle::ID.to_owned(),
            resource: MicrovmPortbHandle { io }.into_resource(),
        });
        chipset_devices.push(ChipsetDeviceHandle {
            name: MicrovmShutdownHandle::ID.to_owned(),
            resource: MicrovmShutdownHandle.into_resource(),
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

        Ok(Some(LoadMode::Linux {
            kernel: kernel.into(),
            initrd: initrd.map(Into::into),
            cmdline: build_effective_microvm_command_line(&opt.cmdline, opt.processors)?,
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

    /// Completes the microVM configuration after the storage devices have been
    /// added, and validates the machine contract of every profile.
    pub(crate) fn finish(self, cfg: &mut Config) -> anyhow::Result<()> {
        let opt = self.opt;
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
        openvmm_defs::microvm::validate_machine_config(cfg, requested_hypervisor)?;
        Ok(())
    }
}

/// Connects the portb endpoint to the host console, relaying its output to
/// stdout, and returns its backend.
fn setup_host_console(
    name: &str,
    device: &'static str,
    console_state: &RefCell<Option<ConsoleState<'static>>>,
    serial_driver: &DefaultDriver,
) -> anyhow::Result<Resource<SerialBackendHandle>> {
    if let Some(console_state) = console_state.borrow().as_ref() {
        bail!("console already set by {}", console_state.device);
    }
    let (config, serial) = serial_io::anonymous_serial_pair(serial_driver)?;
    let (serial_read, serial_write) = AsyncReadExt::split(serial);
    *console_state.borrow_mut() = Some(ConsoleState {
        device,
        input: Box::new(serial_write),
    });
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let _ = block_on(futures::io::copy(
                serial_read,
                &mut AllowStdIo::new(term::raw_stdout()),
            ));
        })
        .context("failed to spawn the portb output relay")?;
    Ok(config)
}

fn build_effective_microvm_command_line(
    user_args: &[String],
    processor_count: u32,
) -> anyhow::Result<String> {
    let mut cmdline = build_microvm_command_line(user_args)?;
    openvmm_defs::microvm::append_microvm_processor_limit(&mut cmdline, processor_count)?;
    Ok(cmdline)
}
