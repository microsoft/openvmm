// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Crate to help build a VM manifest.
//!
//! The VM's _manifest_ is a list of device handles (and, for now, legacy device
//! configuration for [`vmotherboard`]) for devices that are present in a VM.
//!
//! This crate helps build the manifest via the [`VmManifestBuilder`] type. This
//! can be used to construct common manifests for different VM types, such as
//! Hyper-V generation 1 and 2 VMs, unenlightened Linux VMs, and Underhill VMs.
//!
//! For now, this crate only builds handles and configuration for "chipset"
//! devices. In the future, it will also build handles for PCI and VMBus
//! devices.

#![forbid(unsafe_code)]

use chipset_resources::battery::BatteryDeviceHandleAArch64;
use chipset_resources::battery::BatteryDeviceHandleX64;
use chipset_resources::battery::HostBatteryUpdate;
use chipset_resources::i8042::I8042DeviceHandle;
use input_core::MultiplexedInputHandle;
use missing_dev_resources::MissingDevHandle;
use serial_16550_resources::Serial16550DeviceHandle;
use serial_core::resources::DisconnectedSerialBackendHandle;
use serial_debugcon_resources::SerialDebugconDeviceHandle;
use serial_pl011_resources::SerialPl011DeviceHandle;
use std::iter::zip;
use thiserror::Error;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::SerialBackendHandle;
use vmotherboard::ChipsetDeviceHandle;
use vmotherboard::options::BaseChipsetManifest;

/// Builder for a VM manifest.
pub struct VmManifestBuilder {
    ty: BaseChipsetType,
    arch: MachineArch,
    serial: Option<[Option<Resource<SerialBackendHandle>>; 4]>,
    serial_wait_for_rts: bool,
    proxy_vga: bool,
    stub_floppy: bool,
    battery_status_recv: Option<mesh::Receiver<HostBatteryUpdate>>,
    framebuffer: bool,
    guest_watchdog: bool,
    psp: bool,
    debugcon: Option<(Resource<SerialBackendHandle>, u16)>,
}

/// The VM's base chipset type, which determines the set of core devices (such
/// as timers, interrupt controllers, and buses) that are present in the VM.
pub enum BaseChipsetType {
    /// Hyper-V generation 1 VM, with a PCAT firmware and PIIX4 chipset.
    HypervGen1,
    /// Hyper-V generation 2 VM, with a UEFI firmware and no legacy devices.
    HypervGen2Uefi,
    /// Hyper-V generation 2 VM, booting directly from Linux with no legacy
    /// devices.
    HyperVGen2LinuxDirect,
    /// VM hosting an HCL (Underhill) instance, with no architectural devices at
    /// all.
    ///
    /// The HCL will determine the actual devices presented to the guest OS;
    /// this VMM just needs to present the devices needed by the HCL.
    HclHost,
    /// Unenlightened Linux VM, with a PCI bus and basic architectural devices.
    UnenlightenedLinuxDirect,
}

/// The machine architecture of the VM.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MachineArch {
    /// x86_64 (AMD64) architecture.
    X86_64,
    /// AArch64 (ARM64) architecture.
    Aarch64,
}

/// The result of building a VM manifest.
pub struct VmChipsetResult {
    /// The base chipset manifest for the VM.
    pub chipset: BaseChipsetManifest,
    /// The list of chipset devices present in the VM.
    pub chipset_devices: Vec<ChipsetDeviceHandle>,
}

/// Error type for building a VM manifest.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] ErrorInner);

#[derive(Debug, Error)]
enum ErrorInner {
    #[error("unsupported architecture")]
    UnsupportedArch,
    #[error("unsupported serial port count")]
    UnsupportedSerialCount,
    #[error("unsupported debugcon architecture")]
    UnsupportedDebugconArch,
    #[error("wait for RTS not supported with this serial type")]
    WaitForRtsNotSupported,
}

impl VmManifestBuilder {
    /// Create a new VM manifest builder for the given chipset type and
    /// architecture.
    pub fn new(ty: BaseChipsetType, arch: MachineArch) -> Self {
        VmManifestBuilder {
            ty,
            arch,
            serial: None,
            serial_wait_for_rts: false,
            proxy_vga: false,
            stub_floppy: false,
            battery_status_recv: None,
            framebuffer: false,
            guest_watchdog: false,
            psp: false,
            debugcon: None,
        }
    }

    /// Enable serial ports (of a type determined by the chipset type), backed
    /// by the given serial backends.
    ///
    /// For Hyper-V generation 1 VMs, serial ports are always present but are
    /// disconnected unless this method is called. For other VMs, this method
    /// must be called to add serial ports.
    ///
    /// For ARM64 VMs, only two serial ports are supported.
    pub fn with_serial(mut self, serial: [Option<Resource<SerialBackendHandle>>; 4]) -> Self {
        self.serial = Some(serial);
        self
    }

    /// Enable wait-for-RTS mode for serial ports.
    ///
    /// This ensures that the VMM will not push data into the serial port's FIFO
    /// until the guest has raised the RTS line.
    pub fn with_serial_wait_for_rts(mut self) -> Self {
        self.serial_wait_for_rts = true;
        self
    }

    /// Enable the debugcon output-only serial device at the specified port,
    /// backed by the given serial backend.
    ///
    /// Only supported on x86
    pub fn with_debugcon(mut self, serial: Resource<SerialBackendHandle>, port: u16) -> Self {
        self.debugcon = Some((serial, port));
        self
    }

    /// Enable the proxy VGA device.
    ///
    /// This is used for Underhill VMs that are emulating Hyper-V generation 1
    /// VMs.
    pub fn with_proxy_vga(mut self) -> Self {
        assert!(matches!(self.ty, BaseChipsetType::HypervGen1));
        self.proxy_vga = true;
        self
    }

    /// Enable the battery device.
    pub fn with_battery(mut self, battery_status_recv: mesh::Receiver<HostBatteryUpdate>) -> Self {
        self.battery_status_recv = Some(battery_status_recv);
        self
    }

    /// Enable the stub floppy device instead of the full floppy device
    /// implementation.
    ///
    /// This is used to support the saved states for VMs that used the stub
    /// floppy device.
    ///
    /// This is only supported for Hyper-V generation 1 VMs. Panics otherwise.
    pub fn with_stub_floppy(mut self) -> Self {
        assert!(matches!(self.ty, BaseChipsetType::HypervGen1));
        self.stub_floppy = true;
        self
    }

    /// Enable the framebuffer device.
    ///
    /// This is implicit for Hyper-V generation 1 VMs.
    ///
    /// This method will be removed once all devices depending on the
    /// framebuffer are managed through this builder type.
    pub fn with_framebuffer(mut self) -> Self {
        self.framebuffer = true;
        self
    }

    /// Enable the guest watchdog device.
    pub fn with_guest_watchdog(mut self) -> Self {
        self.guest_watchdog = true;
        self
    }

    /// Enable the AMD64 PSP device.
    pub fn with_psp(mut self) -> Self {
        self.psp = true;
        self
    }

    /// Build the VM manifest.
    pub fn build(self) -> Result<VmChipsetResult, Error> {
        let mut result = VmChipsetResult {
            chipset_devices: Vec::new(),
            chipset: BaseChipsetManifest::empty(),
        };

        if let Some((backend, port)) = self.debugcon {
            if matches!(self.arch, MachineArch::X86_64) {
                result.attach_debugcon(port, backend);
            } else {
                return Err(ErrorInner::UnsupportedDebugconArch.into());
            }
        }

        match self.ty {
            BaseChipsetType::HypervGen1 => {
                if self.arch != MachineArch::X86_64 {
                    return Err(Error(ErrorInner::UnsupportedArch));
                }
                result.attach_i8042();
                // This chipset always has a serial port even if not requested.
                result.attach_serial_16550(
                    self.serial_wait_for_rts,
                    self.serial.unwrap_or_else(|| [(); 4].map(|_| None)),
                );
                result.chipset = BaseChipsetManifest {
                    with_generic_cmos_rtc: false,
                    with_generic_ioapic: true,
                    with_generic_isa_dma: true,
                    with_generic_isa_floppy: false,
                    with_generic_pci_bus: false,
                    with_generic_pic: true,
                    with_generic_pit: true,
                    with_generic_psp: false,
                    with_hyperv_firmware_pcat: true,
                    with_hyperv_firmware_uefi: false,
                    with_hyperv_framebuffer: !self.proxy_vga,
                    with_hyperv_guest_watchdog: false,
                    with_hyperv_ide: true,
                    with_hyperv_power_management: false,
                    with_hyperv_vga: !self.proxy_vga,
                    with_i440bx_host_pci_bridge: true,
                    with_piix4_cmos_rtc: true,
                    with_piix4_pci_bus: true,
                    with_piix4_pci_isa_bridge: true,
                    with_piix4_pci_usb_uhci_stub: true,
                    with_piix4_power_management: true,
                    with_underhill_vga_proxy: self.proxy_vga,
                    with_winbond_super_io_and_floppy_stub: self.stub_floppy,
                    with_winbond_super_io_and_floppy_full: !self.stub_floppy,
                };
                result.attach_missing_arch_ports(self.arch, false);
                if let Some(recv) = self.battery_status_recv {
                    result.attach_battery(self.arch, recv);
                }
            }
            BaseChipsetType::UnenlightenedLinuxDirect => {
                let is_x86 = matches!(self.arch, MachineArch::X86_64);
                result.chipset = BaseChipsetManifest {
                    with_generic_cmos_rtc: is_x86,
                    with_generic_ioapic: is_x86,
                    with_generic_isa_dma: false,
                    with_generic_isa_floppy: false,
                    with_generic_pci_bus: is_x86,
                    with_generic_pic: is_x86,
                    with_generic_pit: is_x86,
                    with_generic_psp: self.psp,
                    with_hyperv_firmware_pcat: false,
                    with_hyperv_firmware_uefi: false,
                    with_hyperv_framebuffer: self.framebuffer,
                    with_hyperv_guest_watchdog: self.guest_watchdog,
                    with_hyperv_ide: false,
                    with_hyperv_power_management: is_x86,
                    with_hyperv_vga: false,
                    with_i440bx_host_pci_bridge: false,
                    with_piix4_cmos_rtc: false,
                    with_piix4_pci_bus: false,
                    with_piix4_pci_isa_bridge: false,
                    with_piix4_pci_usb_uhci_stub: false,
                    with_piix4_power_management: false,
                    with_underhill_vga_proxy: false,
                    with_winbond_super_io_and_floppy_stub: false,
                    with_winbond_super_io_and_floppy_full: false,
                };
                result
                    .maybe_attach_arch_serial(
                        self.arch,
                        self.serial_wait_for_rts,
                        true,
                        self.serial,
                    )?
                    .attach_missing_arch_ports(self.arch, false);
                if let Some(recv) = self.battery_status_recv {
                    result.attach_battery(self.arch, recv);
                }
            }
            BaseChipsetType::HypervGen2Uefi | BaseChipsetType::HyperVGen2LinuxDirect => {
                let is_x86 = matches!(self.arch, MachineArch::X86_64);
                result.chipset = BaseChipsetManifest {
                    with_generic_cmos_rtc: is_x86,
                    with_generic_ioapic: is_x86,
                    with_generic_isa_dma: false,
                    with_generic_isa_floppy: false,
                    with_generic_pci_bus: false,
                    with_generic_pic: false,
                    with_generic_pit: false,
                    with_generic_psp: self.psp,
                    with_hyperv_firmware_pcat: false,
                    with_hyperv_firmware_uefi: matches!(self.ty, BaseChipsetType::HypervGen2Uefi),
                    with_hyperv_framebuffer: self.framebuffer,
                    with_hyperv_guest_watchdog: self.guest_watchdog,
                    with_hyperv_ide: false,
                    with_hyperv_power_management: is_x86,
                    with_hyperv_vga: false,
                    with_i440bx_host_pci_bridge: false,
                    with_piix4_cmos_rtc: false,
                    with_piix4_pci_bus: false,
                    with_piix4_pci_isa_bridge: false,
                    with_piix4_pci_usb_uhci_stub: false,
                    with_piix4_power_management: false,
                    with_underhill_vga_proxy: false,
                    with_winbond_super_io_and_floppy_stub: false,
                    with_winbond_super_io_and_floppy_full: false,
                };
                result
                    .maybe_attach_arch_serial(
                        self.arch,
                        self.serial_wait_for_rts,
                        true,
                        self.serial,
                    )?
                    .attach_missing_arch_ports(self.arch, true);
                if let Some(recv) = self.battery_status_recv {
                    result.attach_battery(self.arch, recv);
                }
            }
            BaseChipsetType::HclHost => {
                result.chipset = BaseChipsetManifest {
                    with_hyperv_framebuffer: self.framebuffer,
                    ..BaseChipsetManifest::empty()
                };
                result.maybe_attach_arch_serial(
                    self.arch,
                    self.serial_wait_for_rts,
                    false,
                    self.serial,
                )?;
                if let Some(recv) = self.battery_status_recv {
                    result.attach_battery(self.arch, recv);
                }
            }
        }
        Ok(result)
    }
}

impl VmChipsetResult {
    fn attach_i8042(&mut self) -> &mut Self {
        self.chipset_devices.push(ChipsetDeviceHandle {
            name: "i8042".to_owned(),
            resource: I8042DeviceHandle {
                keyboard_input: MultiplexedInputHandle { elevation: 0 }.into_resource(),
            }
            .into_resource(),
        });
        self
    }

    fn attach_battery(
        &mut self,
        arch: MachineArch,
        battery_status_recv: mesh::Receiver<HostBatteryUpdate>,
    ) -> &mut Self {
        self.chipset_devices.push(ChipsetDeviceHandle {
            name: "battery".to_owned(),
            resource: match arch {
                MachineArch::X86_64 => BatteryDeviceHandleX64 {
                    battery_status_recv,
                }
                .into_resource(),
                MachineArch::Aarch64 => BatteryDeviceHandleAArch64 {
                    battery_status_recv,
                }
                .into_resource(),
            },
        });

        self
    }

    fn maybe_attach_arch_serial(
        &mut self,
        arch: MachineArch,
        wait_for_rts: bool,
        register_missing: bool,
        serial: Option<[Option<Resource<SerialBackendHandle>>; 4]>,
    ) -> Result<&mut Self, ErrorInner> {
        if let Some(serial) = serial {
            match arch {
                MachineArch::X86_64 => {
                    self.attach_serial_16550(wait_for_rts, serial);
                }
                MachineArch::Aarch64 => {
                    if wait_for_rts {
                        return Err(ErrorInner::WaitForRtsNotSupported);
                    }
                    self.attach_serial_pl011(serial)?;
                }
            }
        } else if register_missing && arch == MachineArch::X86_64 {
            self.chipset_devices.push(ChipsetDeviceHandle {
                name: "missing-serial".to_owned(),
                resource: MissingDevHandle::new()
                    .claim_pio("com1", 0x3f8..=0x3ff)
                    .claim_pio("com2", 0x2f8..=0x2ff)
                    .claim_pio("com3", 0x3e8..=0x3ef)
                    .claim_pio("com4", 0x2e8..=0x2ef)
                    .into_resource(),
            });
        }
        Ok(self)
    }

    fn attach_debugcon(&mut self, port: u16, backend: Resource<SerialBackendHandle>) -> &mut Self {
        self.chipset_devices.push(ChipsetDeviceHandle {
            name: format!("debugcon-{port:#x?}"),
            resource: SerialDebugconDeviceHandle { port, io: backend }.into_resource(),
        });
        self
    }

    fn attach_serial_16550(
        &mut self,
        wait_for_rts: bool,
        backends: [Option<Resource<SerialBackendHandle>>; 4],
    ) -> &mut Self {
        let mut devices = Serial16550DeviceHandle::com_ports(
            backends.map(|r| r.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource())),
        );

        if wait_for_rts {
            devices = devices.map(|d| Serial16550DeviceHandle {
                wait_for_rts: true,
                ..d
            });
        }

        self.chipset_devices.extend(
            zip(
                ["serial-com1", "serial-com2", "serial-com3", "serial-com4"],
                devices,
            )
            .map(|(name, device)| ChipsetDeviceHandle {
                name: name.to_string(),
                resource: device.into_resource(),
            }),
        );
        self
    }

    fn attach_serial_pl011(
        &mut self,
        backends: [Option<Resource<SerialBackendHandle>>; 4],
    ) -> Result<&mut Self, ErrorInner> {
        const PL011_SERIAL0_BASE: u64 = 0xEFFEC000;
        const PL011_SERIAL0_IRQ: u32 = 1;
        const PL011_SERIAL1_BASE: u64 = 0xEFFEB000;
        const PL011_SERIAL1_IRQ: u32 = 2;

        let [backend0, backend1, backend2, backend3] = backends;
        if backend2.is_some() || backend3.is_some() {
            return Err(ErrorInner::UnsupportedSerialCount);
        }
        self.chipset_devices.extend([
            ChipsetDeviceHandle {
                name: "com1".to_string(),
                resource: SerialPl011DeviceHandle {
                    base: PL011_SERIAL0_BASE,
                    irq: PL011_SERIAL0_IRQ,
                    io: backend0.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
                }
                .into_resource(),
            },
            ChipsetDeviceHandle {
                name: "com2".to_string(),
                resource: SerialPl011DeviceHandle {
                    base: PL011_SERIAL1_BASE,
                    irq: PL011_SERIAL1_IRQ,
                    io: backend1.unwrap_or_else(|| DisconnectedSerialBackendHandle.into_resource()),
                }
                .into_resource(),
            },
        ]);
        Ok(self)
    }

    fn attach_missing_arch_ports(&mut self, arch: MachineArch, pcat_missing: bool) -> &mut Self {
        if arch != MachineArch::X86_64 {
            return self;
        }

        self.chipset_devices.extend([
            // Some linux versions write to port 0xED as an IO delay mechanims.
            ChipsetDeviceHandle {
                name: "io-delay-0xed".to_owned(),
                resource: MissingDevHandle::new()
                    .claim_pio("delay", 0xed..=0xed)
                    .into_resource(),
            },
            // some windows versions try to unconditionally access these IO ports.
            ChipsetDeviceHandle {
                name: "missing-vmware-backdoor".to_owned(),
                resource: MissingDevHandle::new()
                    .claim_pio("backdoor", 0x5658..=0x5659)
                    .into_resource(),
            },
            // DOS games often unconditionally poll the gameport (e.g: Duke Nukem 1)
            ChipsetDeviceHandle {
                name: "missing-gameport".to_owned(),
                resource: MissingDevHandle::new()
                    .claim_pio("gameport", 0x201..=0x201)
                    .into_resource(),
            },
        ]);

        if pcat_missing {
            self.chipset_devices.extend([
                ChipsetDeviceHandle {
                    name: "missing-pic".to_owned(),
                    resource: MissingDevHandle::new()
                        .claim_pio("primary", 0x20..=0x21)
                        .claim_pio("secondary", 0xa0..=0xa1)
                        .into_resource(),
                },
                ChipsetDeviceHandle {
                    name: "missing-pit".to_owned(),
                    resource: MissingDevHandle::new()
                        .claim_pio("main", 0x40..=0x43)
                        .claim_pio("port61", 0x61..=0x61)
                        .into_resource(),
                },
                ChipsetDeviceHandle {
                    name: "missing-pci".to_owned(),
                    resource: MissingDevHandle::new()
                        .claim_pio("address", 0xcf8..=0xcfb)
                        .claim_pio("data", 0xcfc..=0xcff)
                        .into_resource(),
                },
                // Linux will probe 0x87 during boot to determine if there the DMA
                // device is present
                ChipsetDeviceHandle {
                    name: "missing-dma".to_owned(),
                    resource: MissingDevHandle::new()
                        .claim_pio("io", 0x87..=0x87)
                        .into_resource(),
                },
            ]);
        }
        self
    }
}
