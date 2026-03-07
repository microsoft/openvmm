// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A flexible chipset builder that pre-populates a [`Chipset`](super::Chipset)
//! with a customizable configuration of semi-standardized device.

use crate::ChipsetDeviceHandle;
use crate::PowerEvent;
use crate::chipset::ChipsetBuilder;
use crate::chipset::backing::arc_mutex::device::AddDeviceError;
use crate::chipset::backing::arc_mutex::services::ArcMutexChipsetServices;
use chipset::*;
use chipset_device::interrupt::LineInterruptTarget;
use chipset_device_resources::BSP_LINT_LINE_SET;
use chipset_device_resources::ConfigureChipsetDevice;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use closeable_mutex::CloseableMutex;
use cvm_tracing::CVM_ALLOWED;
use firmware_uefi::UefiCommandSet;
use framebuffer::Framebuffer;
use framebuffer::FramebufferDevice;
use framebuffer::FramebufferLocalControl;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use mesh::MeshPayload;
use state_unit::StateUnits;
use std::fmt::Debug;
use std::sync::Arc;
use thiserror::Error;
use vm_resource::ResourceResolver;
use vmcore::vm_task::VmTaskDriverSource;

/// Errors which may occur during base chipset construction
#[expect(missing_docs)] // error enum with self-describing variants
#[derive(Error, Debug)]
pub enum BaseChipsetBuilderError {
    // transparent + from here is fine, since `AddDeviceError`
    // includes enough context to uniquely identify the source of the error
    #[error(transparent)]
    AddDevice(#[from] AddDeviceError),
    #[error("no valid interrupt controller")]
    MissingInterruptController,
    #[error("attempted to add feature-gated device (requires {0})")]
    FeatureGatedDevice(&'static str),
    #[error("no valid ISA DMA controller for floppy")]
    NoDmaForFloppy,
}

/// A grab-bag of device-specific interfaces that may need to be wired up into
/// upper-layer VMM specific code.
///
/// Fields may or may not be present, depending on what devices were
/// instantiated by the [`BaseChipsetBuilder`]
#[expect(missing_docs)] // self explanatory field names
pub struct BaseChipsetDeviceInterfaces {
    pub framebuffer_local_control: Option<FramebufferLocalControl>,
}

/// A bundle of goodies the base chipset builder returns.
pub struct BaseChipsetBuilderOutput<'a> {
    /// A chipset builder that can be extended with additional devices.
    pub chipset_builder: ChipsetBuilder<'a>,
    /// A collection of device-specific interfaces that may need to be wired up
    /// into upper-layer VMM specific code.
    pub device_interfaces: BaseChipsetDeviceInterfaces,
}

/// A builder that kick-starts Chipset construction by instantiating a bunch of
/// semi-standardized devices.
///
/// i.e: we'd rather not maintain two nearly-identical codepaths to instantiate
/// these devices in both OpenVMM and Underhill.
pub struct BaseChipsetBuilder<'a> {
    foundation: options::BaseChipsetFoundation<'a>,
    devices: options::BaseChipsetDevices,
    device_handles: Vec<ChipsetDeviceHandle>,
    expected_manifest: Option<options::BaseChipsetManifest>,
    fallback_mmio_device: Option<Arc<CloseableMutex<dyn chipset_device::ChipsetDevice>>>,
    flags: BaseChipsetBuilderFlags,
}

struct BaseChipsetBuilderFlags {
    trace_unknown_pio: bool,
    trace_unknown_mmio: bool,
}

impl<'a> BaseChipsetBuilder<'a> {
    /// Create a new [`BaseChipsetBuilder`]
    pub fn new(
        foundation: options::BaseChipsetFoundation<'a>,
        devices: options::BaseChipsetDevices,
    ) -> Self {
        BaseChipsetBuilder {
            foundation,
            devices,
            device_handles: Vec::new(),
            expected_manifest: None,
            fallback_mmio_device: None,
            flags: BaseChipsetBuilderFlags {
                // Legacy OSes have a propensity to blindly access large numbers
                // of unknown IO ports during boot (e.g: as part of ISA OnP
                // device probing). As such, VMM implementations that wish to
                // support Legacy OSes may wish to silence missing pio access
                // warnings.
                //
                // The same is _not_ true for unexpected MMIO intercepts, as a
                // well-behaved OS shouldn't try to read from unclaimed MMIO.
                // Such accesses almost certainly indicate that there's a bug
                // somewhere - be it in our code, or somewhere within the guest.
                // Certain configurations of the VMM may need to support
                // emulating on arbitrary MMIO addresses that back assigned
                // devices, where the address is not known apriori. In such
                // configurations, provide the option to disable mmio tracing.
                trace_unknown_pio: false,
                trace_unknown_mmio: true,
            },
        }
    }

    /// Double-check that the provided [`options::BaseChipsetDevices`] has the
    /// same devices as specified by `expected_manifest`
    pub fn with_expected_manifest(
        mut self,
        expected_manifest: options::BaseChipsetManifest,
    ) -> Self {
        self.expected_manifest = Some(expected_manifest);
        self
    }

    /// Adds device handles to be resolved and instantiated.
    pub fn with_device_handles(mut self, mut device_handles: Vec<ChipsetDeviceHandle>) -> Self {
        self.device_handles.append(&mut device_handles);
        self
    }

    /// Emit "missing device" traces when accessing unknown port IO addresses.
    ///
    /// Disabled by default.
    pub fn with_trace_unknown_pio(mut self, active: bool) -> Self {
        self.flags.trace_unknown_pio = active;
        self
    }

    /// Emit "missing device" traces when accessing unknown port MMIO addresses.
    ///
    /// Enabled by default.
    pub fn with_trace_unknown_mmio(mut self, active: bool) -> Self {
        self.flags.trace_unknown_mmio = active;
        self
    }

    /// Set a fallback MMIO device to be used when no other device claims an
    /// address range.
    pub fn with_fallback_mmio_device(
        mut self,
        fallback_mmio_device: Option<Arc<CloseableMutex<dyn chipset_device::ChipsetDevice>>>,
    ) -> Self {
        self.fallback_mmio_device = fallback_mmio_device;
        self
    }

    /// Create a new base chipset. Returns a [`ChipsetBuilder`] which can be
    /// extended with additional devices, alongside a collection of
    /// [`BaseChipsetDeviceInterfaces`] that will need to be wired up by the
    /// caller.
    pub async fn build(
        self,
        driver_source: &'a VmTaskDriverSource,
        units: &'a StateUnits,
        resolver: &ResourceResolver,
    ) -> Result<BaseChipsetBuilderOutput<'a>, BaseChipsetBuilderError> {
        let Self {
            foundation,
            devices,
            device_handles,
            expected_manifest,
            fallback_mmio_device,
            flags,
        } = self;

        let manifest = devices.to_manifest();
        if let Some(expected_manifest) = expected_manifest {
            assert_eq!(expected_manifest, manifest, "manifests do not match");
        }

        let mut device_interfaces = BaseChipsetDeviceInterfaces {
            framebuffer_local_control: None,
        };

        let mut builder = ChipsetBuilder::new(
            driver_source,
            units,
            foundation.debug_event_handler.clone(),
            foundation.vmtime,
            foundation.vmtime_unit,
            flags.trace_unknown_pio,
            flags.trace_unknown_mmio,
            fallback_mmio_device,
        );

        // oh boy, time to build all the devices!
        let options::BaseChipsetDevices {
            deps_generic_cmos_rtc,
            deps_generic_ioapic,
            deps_generic_isa_dma,
            deps_generic_isa_floppy,
            deps_generic_pci_bus,
            deps_generic_pic,
            deps_generic_pit,
            deps_generic_psp: _, // not actually a device... yet
            deps_hyperv_firmware_pcat,
            deps_hyperv_firmware_uefi,
            deps_hyperv_framebuffer,
            deps_hyperv_guest_watchdog,
            deps_hyperv_ide,
            deps_hyperv_power_management,
            deps_hyperv_vga,
            deps_i440bx_host_pci_bridge,
            deps_piix4_cmos_rtc,
            deps_piix4_pci_bus,
            deps_piix4_pci_isa_bridge,
            deps_piix4_pci_usb_uhci_stub,
            deps_piix4_power_management,
            deps_underhill_vga_proxy,
            deps_winbond_super_io_and_floppy_stub,
            deps_winbond_super_io_and_floppy_full,
        } = devices;

        if let Some(options::dev::GenericPicDeps {}) = deps_generic_pic {
            builder.arc_mutex_device("pic").add(|services| {
                // Map IRQ2 to PIC IRQ0 (used by the PIT), since PIC IRQ2 is used to
                // cascade the secondary PIC's output onto the primary.
                //
                // Don't map IRQ0 at all.
                services.add_line_target(IRQ_LINE_SET, 1..=1, 1);
                services.add_line_target(IRQ_LINE_SET, 2..=2, 0);
                services.add_line_target(IRQ_LINE_SET, 3..=15, 3);

                // Raise interrupt requests by raising the BSP's LINT0.
                pic::DualPic::new(
                    services.new_line(BSP_LINT_LINE_SET, "ready", 0),
                    &mut services.register_pio(),
                )
            })?;
        }

        if let Some(options::dev::GenericIoApicDeps {
            num_entries,
            routing,
        }) = deps_generic_ioapic
        {
            builder.arc_mutex_device("ioapic").add(|services| {
                services.add_line_target(IRQ_LINE_SET, 0..=num_entries as u32 - 1, 0);
                ioapic::IoApicDevice::new(num_entries, routing)
            })?;
        }

        if let Some(options::dev::GenericPciBusDeps {
            bus_id,
            pio_addr,
            pio_data,
        }) = deps_generic_pci_bus
        {
            let pci = builder.arc_mutex_device("pci_bus").add(|services| {
                pci_bus::GenericPciBus::new(&mut services.register_pio(), pio_addr, pio_data)
            })?;

            builder.register_weak_mutex_pci_bus(bus_id, Box::new(pci));
        }

        if let Some(options::dev::Piix4PciBusDeps { bus_id }) = deps_piix4_pci_bus {
            // TODO: use PowerRequestHandleKind
            let reset = {
                let power = foundation.power_event_handler.clone();
                Box::new(move || power.on_power_event(PowerEvent::Reset))
            };

            let pci = builder.arc_mutex_device("piix4-pci-bus").add(|services| {
                chipset_legacy::piix4_pci_bus::Piix4PciBus::new(
                    &mut services.register_pio(),
                    reset.clone(),
                )
            })?;
            builder.register_weak_mutex_pci_bus(bus_id, Box::new(pci));
        }

        if let Some(options::dev::I440BxHostPciBridgeDeps {
            attached_to,
            adjust_gpa_range,
        }) = deps_i440bx_host_pci_bridge
        {
            builder
                .arc_mutex_device("440bx-host-pci-bridge")
                .on_pci_bus(attached_to)
                .add(|_| {
                    chipset_legacy::i440bx_host_pci_bridge::HostPciBridge::new(
                        adjust_gpa_range,
                        foundation.is_restoring,
                    )
                })?;
        }

        let dma = {
            if let Some(options::dev::GenericIsaDmaDeps {}) = deps_generic_isa_dma {
                let dma = builder
                    .arc_mutex_device("dma")
                    .add(|_| dma::DmaController::new())?;
                Some(dma)
            } else {
                None
            }
        };

        if let Some(options::dev::Piix4PciIsaBridgeDeps { attached_to }) = deps_piix4_pci_isa_bridge
        {
            // TODO: use PowerRequestHandleKind
            let reset = {
                let power = foundation.power_event_handler.clone();
                Box::new(move || power.on_power_event(PowerEvent::Reset))
            };

            let set_a20_signal = Box::new(move |active| {
                tracing::info!(CVM_ALLOWED, active, "setting stubbed A20 signal")
            });

            builder
                .arc_mutex_device("piix4-pci-isa-bridge")
                .on_pci_bus(attached_to)
                .add(|_| {
                    chipset_legacy::piix4_pci_isa_bridge::PciIsaBridge::new(
                        reset.clone(),
                        set_a20_signal,
                    )
                })?;
        }

        if let Some(options::dev::Piix4PciUsbUhciStubDeps { attached_to }) =
            deps_piix4_pci_usb_uhci_stub
        {
            builder
                .arc_mutex_device("piix4-usb-uhci-stub")
                .on_pci_bus(attached_to)
                .add(|_| chipset_legacy::piix4_uhci::Piix4UsbUhciStub::new())?;
        }

        if let Some(options::dev::GenericPitDeps {}) = deps_generic_pit {
            // hard-coded IRQ lines, as per x86 spec
            builder.arc_mutex_device("pit").add(|services| {
                pit::PitDevice::new(
                    services.new_line(IRQ_LINE_SET, "timer0", 2),
                    services.register_vmtime().access("pit"),
                )
            })?;
        }

        let _ = dma;
        #[cfg(feature = "dev_generic_isa_floppy")]
        if let Some(options::dev::GenericIsaFloppyDeps {
            irq,
            dma_channel: dma_chan,
            pio_base,
            drives,
        }) = deps_generic_isa_floppy
        {
            if let Some(dma) = &dma {
                let dma_channel = ArcMutexIsaDmaChannel::new(dma.clone(), dma_chan);

                builder.arc_mutex_device("floppy").try_add(|services| {
                    let interrupt = services.new_line(IRQ_LINE_SET, "interrupt", irq);
                    floppy::FloppyDiskController::new(
                        foundation.untrusted_dma_memory.clone(),
                        interrupt,
                        &mut services.register_pio(),
                        pio_base,
                        drives,
                        Box::new(dma_channel),
                    )
                })?;
            } else {
                return Err(BaseChipsetBuilderError::NoDmaForFloppy);
            }
        }

        #[cfg(feature = "dev_winbond_super_io_and_floppy_full")]
        if let Some(options::dev::WinbondSuperIoAndFloppyFullDeps {
            primary_disk_drive,
            secondary_disk_drive,
        }) = deps_winbond_super_io_and_floppy_full
        {
            if let Some(dma) = &dma {
                // IRQ and DMA channel assignment MUST match the values reported
                // by the PCAT BIOS ACPI tables, and the Super IO emulator.
                let primary_dma = Box::new(ArcMutexIsaDmaChannel::new(dma.clone(), 2));
                let secondary_dma = Box::new(vmcore::isa_dma_channel::FloatingDmaChannel);

                builder.arc_mutex_device("floppy-sio").try_add(|services| {
                    let interrupt = services.new_line(IRQ_LINE_SET, "interrupt", 6);
                    chipset_legacy::winbond83977_sio::Winbond83977FloppySioDevice::<
                        floppy::FloppyDiskController,
                    >::new(
                        foundation.untrusted_dma_memory.clone(),
                        interrupt,
                        &mut services.register_pio(),
                        primary_disk_drive,
                        secondary_disk_drive,
                        primary_dma,
                        secondary_dma,
                    )
                })?;
            } else {
                return Err(BaseChipsetBuilderError::NoDmaForFloppy);
            }
        }

        #[cfg(feature = "dev_winbond_super_io_and_floppy_stub")]
        if let Some(options::dev::WinbondSuperIoAndFloppyStubDeps) =
            deps_winbond_super_io_and_floppy_stub
        {
            if let Some(dma) = &dma {
                // IRQ and DMA channel assignment MUST match the values reported
                // by the PCAT BIOS ACPI tables, and the Super IO emulator.
                let primary_dma = Box::new(ArcMutexIsaDmaChannel::new(dma.clone(), 2));
                let secondary_dma = Box::new(vmcore::isa_dma_channel::FloatingDmaChannel);

                builder.arc_mutex_device("floppy-sio").try_add(|services| {
                    let interrupt = services.new_line(IRQ_LINE_SET, "interrupt", 6);
                    chipset_legacy::winbond83977_sio::Winbond83977FloppySioDevice::<
                        floppy_pcat_stub::StubFloppyDiskController,
                    >::new(
                        foundation.untrusted_dma_memory.clone(),
                        interrupt,
                        &mut services.register_pio(),
                        floppy::DriveRibbon::None,
                        floppy::DriveRibbon::None,
                        primary_dma,
                        secondary_dma,
                    )
                })?;
            } else {
                return Err(BaseChipsetBuilderError::NoDmaForFloppy);
            }
        }

        if let Some(options::dev::HyperVIdeDeps {
            attached_to,
            primary_channel_drives,
            secondary_channel_drives,
        }) = deps_hyperv_ide
        {
            builder
                .arc_mutex_device("ide")
                .on_pci_bus(attached_to)
                .try_add(|services| {
                    // hard-coded to iRQ lines 14 and 15, as per PIIX4 spec
                    let primary_channel_line_interrupt =
                        services.new_line(IRQ_LINE_SET, "ide1", 14);
                    let secondary_channel_line_interrupt =
                        services.new_line(IRQ_LINE_SET, "ide2", 15);
                    ide::IdeDevice::new(
                        foundation.untrusted_dma_memory.clone(),
                        &mut services.register_pio(),
                        primary_channel_drives,
                        secondary_channel_drives,
                        primary_channel_line_interrupt,
                        secondary_channel_line_interrupt,
                    )
                })?;
        }

        if let Some(options::dev::GenericCmosRtcDeps {
            irq,
            time_source,
            century_reg_idx,
            initial_cmos,
        }) = deps_generic_cmos_rtc
        {
            builder.arc_mutex_device("rtc").add(|services| {
                cmos_rtc::Rtc::new(
                    time_source,
                    services.new_line(IRQ_LINE_SET, "interrupt", irq),
                    services.register_vmtime(),
                    century_reg_idx,
                    initial_cmos,
                    false,
                )
            })?;
        }

        if let Some(options::dev::Piix4CmosRtcDeps {
            time_source,
            initial_cmos,
            enlightened_interrupts,
        }) = deps_piix4_cmos_rtc
        {
            builder.arc_mutex_device("piix4-rtc").add(|services| {
                // hard-coded to IRQ line 8, as per PIIX4 spec
                let rtc_interrupt = services.new_line(IRQ_LINE_SET, "interrupt", 8);
                chipset_legacy::piix4_cmos_rtc::Piix4CmosRtc::new(
                    time_source,
                    rtc_interrupt,
                    services.register_vmtime(),
                    initial_cmos,
                    enlightened_interrupts,
                )
            })?;
        }

        // The ACPI GPE0 line to use for generation ID. This must match the
        // value in the DSDT.
        const GPE0_LINE_GENERATION_ID: u32 = 0;
        // for ARM64, 3 + 32 (SPI range start) = 35,
        // the SYSTEM_SPI_GENCOUNTER vector for the GIC
        const GENERATION_ID_IRQ: u32 = 3;

        // TODO: use PowerRequestHandleKind
        let pm_action = || {
            let power = foundation.power_event_handler.clone();
            move |action: pm::PowerAction| {
                tracing::info!(CVM_ALLOWED, ?action, "guest initiated");
                let req = match action {
                    pm::PowerAction::PowerOff => PowerEvent::PowerOff,
                    pm::PowerAction::Hibernate => PowerEvent::Hibernate,
                    pm::PowerAction::Reboot => PowerEvent::Reset,
                };
                power.on_power_event(req);
            }
        };

        if let Some(options::dev::HyperVPowerManagementDeps {
            acpi_irq,
            pio_base: pio_dynamic_reg_base,
            pm_timer_assist,
        }) = deps_hyperv_power_management
        {
            builder.arc_mutex_device("pm").add(|services| {
                let pm = pm::PowerManagementDevice::new(
                    Box::new(pm_action()),
                    services.new_line(IRQ_LINE_SET, "gpe0", acpi_irq),
                    &mut services.register_pio(),
                    services.register_vmtime().access("pm"),
                    Some(pm::EnableAcpiMode {
                        default_pio_dynamic: pio_dynamic_reg_base,
                    }),
                    pm_timer_assist,
                );
                for range in pm.valid_lines() {
                    services.add_line_target(GPE0_LINE_SET, range.clone(), *range.start());
                }
                pm
            })?;
        }

        if let Some(options::dev::Piix4PowerManagementDeps {
            attached_to,
            pm_timer_assist,
        }) = deps_piix4_power_management
        {
            builder
                .arc_mutex_device("piix4-pm")
                .on_pci_bus(attached_to)
                .add(|services| {
                    // hard-coded to IRQ line 9, as per PIIX4 spec
                    let interrupt = services.new_line(IRQ_LINE_SET, "acpi", 9);
                    let pm = chipset_legacy::piix4_pm::Piix4Pm::new(
                        Box::new(pm_action()),
                        interrupt,
                        &mut services.register_pio(),
                        services.register_vmtime().access("piix4-pm"),
                        pm_timer_assist,
                    );
                    for range in pm.valid_lines() {
                        services.add_line_target(GPE0_LINE_SET, range.clone(), *range.start());
                    }
                    pm
                })?;
        }

        if let Some(options::dev::HyperVGuestWatchdogDeps {
            watchdog_platform,
            port_base: pio_wdat_port,
        }) = deps_hyperv_guest_watchdog
        {
            builder
                .arc_mutex_device("guest-watchdog")
                .add_async(async |services| {
                    let vmtime = services.register_vmtime();
                    let mut register_pio = services.register_pio();
                    guest_watchdog::GuestWatchdogServices::new(
                        vmtime.access("guest-watchdog-time"),
                        watchdog_platform,
                        &mut register_pio,
                        pio_wdat_port,
                        foundation.is_restoring,
                    )
                    .await
                })
                .await?;
        }

        if let Some(options::dev::HyperVFirmwareUefi {
            config,
            logger,
            nvram_storage,
            generation_id_recv,
            watchdog_platform,
            watchdog_recv,
            vsm_config,
            time_source,
        }) = deps_hyperv_firmware_uefi
        {
            builder
                .arc_mutex_device("uefi")
                .try_add_async(async |services| {
                    let notify_interrupt = match config.command_set {
                        UefiCommandSet::X64 => {
                            services.new_line(GPE0_LINE_SET, "genid", GPE0_LINE_GENERATION_ID)
                        }
                        UefiCommandSet::Aarch64 => {
                            services.new_line(IRQ_LINE_SET, "genid", GENERATION_ID_IRQ)
                        }
                    };
                    let vmtime = services.register_vmtime();
                    let gm = foundation.trusted_vtl0_dma_memory.clone();
                    let runtime_deps = firmware_uefi::UefiRuntimeDeps {
                        gm: gm.clone(),
                        nvram_storage,
                        logger,
                        vmtime,
                        watchdog_platform,
                        watchdog_recv,
                        generation_id_deps: generation_id::GenerationIdRuntimeDeps {
                            generation_id_recv,
                            gm,
                            notify_interrupt,
                        },
                        vsm_config,
                        time_source,
                    };

                    firmware_uefi::UefiDevice::new(runtime_deps, config, foundation.is_restoring)
                        .await
                })
                .await?;
        }

        if let Some(options::dev::HyperVFirmwarePcat {
            config,
            logger,
            generation_id_recv,
            rom,
            replay_mtrrs,
        }) = deps_hyperv_firmware_pcat
        {
            builder.arc_mutex_device("pcat").try_add(|services| {
                let notify_interrupt =
                    services.new_line(GPE0_LINE_SET, "genid", GPE0_LINE_GENERATION_ID);
                firmware_pcat::PcatBiosDevice::new(
                    firmware_pcat::PcatBiosRuntimeDeps {
                        gm: foundation.trusted_vtl0_dma_memory.clone(),
                        logger,
                        generation_id_deps: generation_id::GenerationIdRuntimeDeps {
                            generation_id_recv,
                            gm: foundation.trusted_vtl0_dma_memory.clone(),
                            notify_interrupt,
                        },
                        vmtime: services.register_vmtime(),
                        rom,
                        register_pio: &mut services.register_pio(),
                        replay_mtrrs,
                    },
                    config,
                )
            })?;
        }

        if let Some(options::dev::HyperVFramebufferDeps {
            fb_mapper,
            fb,
            vtl2_framebuffer_gpa_base,
        }) = deps_hyperv_framebuffer
        {
            let fb = FramebufferDevice::new(fb_mapper, fb, vtl2_framebuffer_gpa_base);
            let control = fb.as_ref().ok().map(|fb| fb.control());
            builder.arc_mutex_device("fb").try_add(|_| fb)?;
            device_interfaces.framebuffer_local_control = Some(control.unwrap());
        }

        #[cfg(feature = "dev_hyperv_vga")]
        if let Some(options::dev::HyperVVgaDeps { attached_to, rom }) = deps_hyperv_vga {
            builder
                .arc_mutex_device("vga")
                .on_pci_bus(attached_to)
                .try_add(|services| {
                    vga::VgaDevice::new(
                        &driver_source.simple(),
                        services.register_vmtime(),
                        device_interfaces.framebuffer_local_control.clone().unwrap(),
                        rom,
                    )
                })?;
        }

        #[cfg(feature = "dev_underhill_vga_proxy")]
        if let Some(options::dev::UnderhillVgaProxyDeps {
            attached_to,
            pci_cfg_proxy,
            register_host_io_fastpath,
        }) = deps_underhill_vga_proxy
        {
            builder
                .arc_mutex_device("vga_proxy")
                .on_pci_bus(attached_to)
                .add(|_services| {
                    vga_proxy::VgaProxyDevice::new(pci_cfg_proxy, &*register_host_io_fastpath)
                })?;
        }

        macro_rules! feature_gate_check {
            ($feature:literal, $dep:ident) => {
                #[cfg(not(feature = $feature))]
                let None::<()> = $dep else {
                    return Err(BaseChipsetBuilderError::FeatureGatedDevice($feature));
                };
            };
        }

        feature_gate_check!("dev_hyperv_vga", deps_hyperv_vga);
        feature_gate_check!("dev_underhill_vga_proxy", deps_underhill_vga_proxy);
        feature_gate_check!("dev_generic_isa_floppy", deps_generic_isa_floppy);
        feature_gate_check!(
            "dev_winbond_super_io_and_floppy_full",
            deps_winbond_super_io_and_floppy_full
        );
        feature_gate_check!(
            "dev_winbond_super_io_and_floppy_stub",
            deps_winbond_super_io_and_floppy_stub
        );

        for device in device_handles {
            builder
                .arc_mutex_device(device.name.as_ref())
                .try_add_async(async |services| {
                    resolver
                        .resolve(
                            device.resource,
                            ResolveChipsetDeviceHandleParams {
                                device_name: device.name.as_ref(),
                                guest_memory: &foundation.untrusted_dma_memory,
                                encrypted_guest_memory: &foundation.trusted_vtl0_dma_memory,
                                vmtime: foundation.vmtime,
                                is_restoring: foundation.is_restoring,
                                task_driver_source: driver_source,
                                register_mmio: &mut services.register_mmio(),
                                register_pio: &mut services.register_pio(),
                                configure: services,
                            },
                        )
                        .await
                        .map(|dev| dev.0)
                })
                .await?;
        }

        Ok(BaseChipsetBuilderOutput {
            chipset_builder: builder,
            device_interfaces,
        })
    }
}

impl ConfigureChipsetDevice for ArcMutexChipsetServices<'_, '_> {
    fn new_line(
        &mut self,
        id: chipset_device_resources::LineSetId,
        name: &str,
        vector: u32,
    ) -> vmcore::line_interrupt::LineInterrupt {
        self.new_line(id, name, vector)
    }

    fn add_line_target(
        &mut self,
        id: chipset_device_resources::LineSetId,
        source_range: std::ops::RangeInclusive<u32>,
        target_start: u32,
    ) {
        self.add_line_target(id, source_range, target_start)
    }

    fn omit_saved_state(&mut self) {
        self.omit_saved_state();
    }
}

mod weak_mutex_pci {
    use crate::chipset::PciConflict;
    use crate::chipset::PciConflictReason;
    use crate::chipset::PcieConflict;
    use crate::chipset::PcieConflictReason;
    use crate::chipset::backing::arc_mutex::pci::RegisterWeakMutexPci;
    use crate::chipset::backing::arc_mutex::pci::RegisterWeakMutexPcie;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use closeable_mutex::CloseableMutex;
    use pci_bus::GenericPciBusDevice;
    use std::sync::Arc;
    use std::sync::Weak;

    /// Wrapper around `Weak<CloseableMutex<dyn ChipsetDevice>>` that implements
    /// [`GenericPciBusDevice`]
    pub struct WeakMutexPciDeviceWrapper(Weak<CloseableMutex<dyn ChipsetDevice>>);

    impl GenericPciBusDevice for WeakMutexPciDeviceWrapper {
        fn pci_cfg_read(&mut self, offset: u16, value: &mut u32) -> Option<IoResult> {
            Some(
                self.0
                    .upgrade()?
                    .lock()
                    .supports_pci()
                    .expect("builder code ensures supports_pci.is_some()")
                    .pci_cfg_read(offset, value),
            )
        }

        fn pci_cfg_write(&mut self, offset: u16, value: u32) -> Option<IoResult> {
            Some(
                self.0
                    .upgrade()?
                    .lock()
                    .supports_pci()
                    .expect("builder code ensures supports_pci.is_some()")
                    .pci_cfg_write(offset, value),
            )
        }

        fn pci_cfg_read_forward(
            &mut self,
            bus: u8,
            device_function: u8,
            offset: u16,
            value: &mut u32,
        ) -> Option<IoResult> {
            self.0
                .upgrade()?
                .lock()
                .supports_pci()
                .expect("builder code ensures supports_pci.is_some()")
                .pci_cfg_read_forward(bus, device_function, offset, value)
        }

        fn pci_cfg_write_forward(
            &mut self,
            bus: u8,
            device_function: u8,
            offset: u16,
            value: u32,
        ) -> Option<IoResult> {
            self.0
                .upgrade()?
                .lock()
                .supports_pci()
                .expect("builder code ensures supports_pci.is_some()")
                .pci_cfg_write_forward(bus, device_function, offset, value)
        }
    }

    // wiring to enable using the generic PCI bus alongside the Arc+CloseableMutex device infra
    impl RegisterWeakMutexPci for Arc<CloseableMutex<pci_bus::GenericPciBus>> {
        fn add_pci_device(
            &mut self,
            bus: u8,
            device: u8,
            function: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PciConflict> {
            self.lock()
                .add_pci_device(
                    bus,
                    device,
                    function,
                    name.clone(),
                    WeakMutexPciDeviceWrapper(dev),
                )
                .map_err(|occ_err| PciConflict {
                    bdf: (bus, device, function),
                    reason: PciConflictReason::ExistingDev(occ_err.existing_device_name),
                    conflict_dev: name,
                })
        }
    }

    // wiring to enable using the PIIX4 PCI bus alongside the Arc+CloseableMutex device infra
    impl RegisterWeakMutexPci for Arc<CloseableMutex<chipset_legacy::piix4_pci_bus::Piix4PciBus>> {
        fn add_pci_device(
            &mut self,
            bus: u8,
            device: u8,
            function: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PciConflict> {
            self.lock()
                .as_pci_bus()
                .add_pci_device(
                    bus,
                    device,
                    function,
                    name.clone(),
                    WeakMutexPciDeviceWrapper(dev),
                )
                .map_err(|occ_err| PciConflict {
                    bdf: (bus, device, function),
                    reason: PciConflictReason::ExistingDev(occ_err.existing_device_name),
                    conflict_dev: name,
                })
        }
    }

    // wiring to enable using the generic PCIe root port alongside the Arc+CloseableMutex device infra
    impl RegisterWeakMutexPcie for Arc<CloseableMutex<pcie::root::GenericPcieRootComplex>> {
        fn add_pcie_device(
            &mut self,
            port: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.lock()
                .add_pcie_device(port, name.clone(), Box::new(WeakMutexPciDeviceWrapper(dev)))
                .map_err(|existing_dev_name| PcieConflict {
                    reason: PcieConflictReason::ExistingDev(existing_dev_name),
                    conflict_dev: name,
                })
        }

        fn downstream_ports(&self) -> Vec<(u8, Arc<str>)> {
            self.lock().downstream_ports()
        }
    }

    // wiring to enable using the PCIe switch alongside the Arc+CloseableMutex device infra
    impl RegisterWeakMutexPcie for Arc<CloseableMutex<pcie::switch::GenericPcieSwitch>> {
        fn add_pcie_device(
            &mut self,
            port: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.lock()
                .add_pcie_device(port, &name, Box::new(WeakMutexPciDeviceWrapper(dev)))
                .map_err(|err| PcieConflict {
                    reason: PcieConflictReason::ExistingDev(err.to_string().into()),
                    conflict_dev: name,
                })
        }

        fn downstream_ports(&self) -> Vec<(u8, Arc<str>)> {
            self.lock().downstream_ports()
        }
    }
}

pub struct ArcMutexIsaDmaChannel {
    channel_num: u8,
    dma: Arc<CloseableMutex<dma::DmaController>>,
}

impl ArcMutexIsaDmaChannel {
    #[allow(dead_code)] // use is feature dependent
    pub fn new(dma: Arc<CloseableMutex<dma::DmaController>>, channel_num: u8) -> Self {
        Self { dma, channel_num }
    }
}

impl vmcore::isa_dma_channel::IsaDmaChannel for ArcMutexIsaDmaChannel {
    fn check_transfer_size(&mut self) -> u16 {
        self.dma.lock().check_transfer_size(self.channel_num.into())
    }

    fn request(
        &mut self,
        direction: vmcore::isa_dma_channel::IsaDmaDirection,
    ) -> Option<vmcore::isa_dma_channel::IsaDmaBuffer> {
        self.dma.lock().request(self.channel_num.into(), direction)
    }

    fn complete(&mut self) {
        self.dma.lock().complete(self.channel_num.into())
    }
}

/// [`BaseChipsetBuilder`] options and configuration
pub mod options {
    use super::*;
    use state_unit::UnitHandle;
    use vmcore::vmtime::VmTimeSource;

    /// Foundational `BaseChipset` dependencies (read: not device-specific)
    #[expect(missing_docs)] // self explanatory field names
    pub struct BaseChipsetFoundation<'a> {
        pub is_restoring: bool,
        /// Guest memory access for untrusted devices.
        ///
        /// This should provide access only to memory that is also accessible by
        /// the host. This applies to most devices, where the guest does not
        /// expect that they are implemented by a paravisor.
        ///
        /// If a device incorrectly uses this instead of
        /// `trusted_vtl0_dma_memory`, then it will likely see failures when
        /// accessing guest memory in confidential VM configurations. A
        /// malicious host could additionally use this conspire to observe
        /// trusted device interactions.
        pub untrusted_dma_memory: GuestMemory,
        /// Guest memory access for trusted devices.
        ///
        /// This should provide access to all of VTL0 memory (but not VTL1
        /// memory). This applies to devices that the guest expects to be
        /// implemented by a paravisor, such as security and firmware devices.
        ///
        /// If a device incorrectly uses this instead of `untrusted_dma_memory`,
        /// then it will likely see failures when accessing guest memory in
        /// confidential VM configurations. If the device is under control of a
        /// malicious host in some way, this could also lead to the host
        /// observing encrypted memory.
        pub trusted_vtl0_dma_memory: GuestMemory,
        pub power_event_handler: Arc<dyn crate::PowerEventHandler>,
        pub debug_event_handler: Arc<dyn crate::DebugEventHandler>,
        pub vmtime: &'a VmTimeSource,
        pub vmtime_unit: &'a UnitHandle,
        pub doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    }

    macro_rules! base_chipset_devices_and_manifest {
        (
            // doing this kind of "pseudo-syntax" isn't strictly necessary, but
            // it serves as a nice bit of visual ✨flair✨ that makes it easier
            // to grok what the macro is actually emitting
            impls {
                $(#[$m:meta])*
                pub struct $base_chipset_devices:ident {
                    ...
                }

                $(#[$m2:meta])*
                pub struct $base_chipset_manifest:ident {
                    ...
                }
            }

            devices {
                $($name:ident: $ty:ty,)*
            }
        ) => {paste::paste!{
            $(#[$m])*
            pub struct $base_chipset_devices {
                $(pub [<deps_ $name>]: Option<$ty>,)*
            }

            $(#[$m2])*
            pub struct $base_chipset_manifest {
                $(pub [<with_ $name>]: bool,)*
            }

            impl $base_chipset_manifest {
                /// Return a [`BaseChipsetManifest`] with all fields set to
                /// `false`
                pub const fn empty() -> Self {
                    Self {
                        $([<with_ $name>]: false,)*
                    }
                }
            }

            impl $base_chipset_devices {
                /// Return a [`BaseChipsetDevices`] with all fields set to
                /// `None`
                pub fn empty() -> Self {
                    Self {
                        $([<deps_ $name>]: None,)*
                    }
                }

                /// Return the corresponding [`BaseChipsetManifest`].
                pub fn to_manifest(&self) -> $base_chipset_manifest {
                    let Self {
                        $([<deps_ $name>],)*
                    } = self;

                    $base_chipset_manifest {
                        $([<with_ $name>]: [<deps_ $name>].is_some(),)*
                    }
                }
            }
        }};
    }

    base_chipset_devices_and_manifest! {
        impls {
            /// Device-specific `BaseChipset` dependencies
            #[expect(missing_docs)] // self explanatory field names
            pub struct BaseChipsetDevices {
                // generated struct has fields that look like this:
                //
                // deps_<device>: Option<dev::<Deps>>,
                ...
            }

            /// A manifest of devices specified by [`BaseChipsetDevices`].
            #[expect(missing_docs)] // self explanatory field names
            #[derive(Debug, Clone, MeshPayload, PartialEq, Eq)]
            pub struct BaseChipsetManifest {
                // generated struct has fields that look like this:
                //
                // with_<device>: bool,
                ...
            }
        }

        devices {
            generic_cmos_rtc:            dev::GenericCmosRtcDeps,
            generic_ioapic:              dev::GenericIoApicDeps,
            generic_isa_dma:             dev::GenericIsaDmaDeps,
            generic_isa_floppy:          dev::GenericIsaFloppyDeps,
            generic_pci_bus:             dev::GenericPciBusDeps,
            generic_pic:                 dev::GenericPicDeps,
            generic_pit:                 dev::GenericPitDeps,
            generic_psp:                 dev::GenericPspDeps,

            hyperv_firmware_pcat:        dev::HyperVFirmwarePcat,
            hyperv_firmware_uefi:        dev::HyperVFirmwareUefi,
            hyperv_framebuffer:          dev::HyperVFramebufferDeps,
            hyperv_guest_watchdog:       dev::HyperVGuestWatchdogDeps,
            hyperv_ide:                  dev::HyperVIdeDeps,
            hyperv_power_management:     dev::HyperVPowerManagementDeps,
            hyperv_vga:                  dev::HyperVVgaDeps,

            i440bx_host_pci_bridge:      dev::I440BxHostPciBridgeDeps,

            piix4_cmos_rtc:              dev::Piix4CmosRtcDeps,
            piix4_pci_bus:               dev::Piix4PciBusDeps,
            piix4_pci_isa_bridge:        dev::Piix4PciIsaBridgeDeps,
            piix4_pci_usb_uhci_stub:     dev::Piix4PciUsbUhciStubDeps,
            piix4_power_management:      dev::Piix4PowerManagementDeps,

            underhill_vga_proxy:         dev::UnderhillVgaProxyDeps,

            winbond_super_io_and_floppy_stub: dev::WinbondSuperIoAndFloppyStubDeps,
            winbond_super_io_and_floppy_full: dev::WinbondSuperIoAndFloppyFullDeps,
        }
    }

    /// Device specific dependencies
    pub mod dev {
        use super::*;
        use crate::BusIdPci;
        use chipset_resources::battery::HostBatteryUpdate;
        use local_clock::InspectableLocalClock;

        macro_rules! feature_gated {
            (
                feature = $feat:literal;

                $(#[$m:meta])*
                pub struct $root_deps:ident $($rest:tt)*
            ) => {
                #[cfg(not(feature = $feat))]
                #[doc(hidden)]
                pub type $root_deps = ();

                #[cfg(feature = $feat)]
                $(#[$m])*
                pub struct $root_deps $($rest)*
            };
        }

        /// PIIX4 PCI-ISA bridge (fixed pci address: 0:7.0)
        pub struct Piix4PciIsaBridgeDeps {
            /// `vmotherboard` bus identifier
            pub attached_to: BusIdPci,
        }

        /// Hyper-V IDE controller (fixed pci address: 0:7.1)
        // TODO: this device needs to be broken down further, into a PIIX4 IDE
        // device (without the Hyper-V enlightenments), and then a Generic IDE
        // device (without any of the PIIX4 bus mastering stuff).
        pub struct HyperVIdeDeps {
            /// `vmotherboard` bus identifier
            pub attached_to: BusIdPci,
            /// Drives attached to the primary IDE channel
            pub primary_channel_drives: [Option<ide::DriveMedia>; 2],
            /// Drives attached to the secondary IDE channel
            pub secondary_channel_drives: [Option<ide::DriveMedia>; 2],
        }

        /// PIIX4 USB UHCI controller (fixed pci address: 0:7.2)
        ///
        /// NOTE: current implementation is a minimal stub, implementing just
        /// enough to keep the PCAT BIOS happy.
        pub struct Piix4PciUsbUhciStubDeps {
            /// `vmotherboard` bus identifier
            pub attached_to: BusIdPci,
        }

        /// PIIX4 power management device (fixed pci address: 0:7.3)
        pub struct Piix4PowerManagementDeps {
            /// `vmotherboard` bus identifier
            pub attached_to: BusIdPci,
            /// Interface to enable/disable PM timer assist
            pub pm_timer_assist: Option<Box<dyn pm::PmTimerAssist>>,
        }

        /// Generic dual 8237A ISA DMA controllers
        pub struct GenericIsaDmaDeps;

        /// Hyper-V specific ACPI-compatible power management device
        pub struct HyperVPowerManagementDeps {
            /// IRQ line triggered on ACPI power event
            pub acpi_irq: u32,
            /// Base port io address of the device's register region
            pub pio_base: u16,
            /// Interface to enable/disable PM timer assist
            pub pm_timer_assist: Option<Box<dyn pm::PmTimerAssist>>,
        }

        /// AMD Platform Security Processor (PSP)
        pub struct GenericPspDeps;

        feature_gated! {
            feature = "dev_generic_isa_floppy";

            /// Generic ISA floppy controller
            pub struct GenericIsaFloppyDeps {
                /// IRQ line shared by both floppy controllers
                pub irq: u32,
                /// DMA channel to use for floppy DMA transfers
                pub dma_channel: u8,
                /// Base port io address of the primary devices's register region
                pub pio_base: u16,
                /// Floppy Drives attached to the controller
                pub drives: floppy::DriveRibbon,
            }
        }

        feature_gated! {
            feature = "dev_winbond_super_io_and_floppy_stub";

            /// Stub Winbond83977 "Super I/O" chip + dual-floppy controllers
            ///
            /// Unconditionally reports no connected floppy drives. Useful for
            /// VMMs that wish to support BIOS boot via the Microsoft PCAT
            /// firmware, without paying the binary size + complexity cost of a
            /// full floppy disk controller implementation.
            ///
            /// IRQ and DMA channel assignment MUST match the values reported by
            /// the PCAT BIOS ACPI tables, and the Super IO emulator, and cannot
            /// be tweaked by top-level VMM code.
            pub struct WinbondSuperIoAndFloppyStubDeps;
        }

        feature_gated! {
            feature = "dev_winbond_super_io_and_floppy_full";

            /// Winbond83977 "Super I/O" chip + dual-floppy controllers
            ///
            /// IRQ and DMA channel assignment MUST match the values reported by the
            /// PCAT BIOS ACPI tables, and the Super IO emulator, and cannot be
            /// tweaked by top-level VMM code.
            pub struct WinbondSuperIoAndFloppyFullDeps {
                /// Floppy Drive attached to the primary controller
                pub primary_disk_drive: floppy::DriveRibbon,
                /// Floppy Drive attached to the secondary controller
                pub secondary_disk_drive: floppy::DriveRibbon,
            }
        }

        /// Generic PCI bus
        pub struct GenericPciBusDeps {
            /// `vmotherboard` bus identifier
            pub bus_id: BusIdPci,
            /// Port io address of the 32-bit PCI ADDR register
            pub pio_addr: u16,
            /// Port io address of the 32-bit PCI DATA register
            pub pio_data: u16,
        }

        /// PIIX4 PCI Bus
        pub struct Piix4PciBusDeps {
            /// `vmotherboard` bus identifier
            pub bus_id: BusIdPci,
        }

        /// i440BX Host-PCI bridge (fixed pci address: 0:0.0)
        pub struct I440BxHostPciBridgeDeps {
            /// `vmotherboard` bus identifier
            pub attached_to: BusIdPci,
            /// Interface to create GPA alias ranges.
            pub adjust_gpa_range: Box<dyn chipset_legacy::i440bx_host_pci_bridge::AdjustGpaRange>,
        }

        /// Generic Intel 8253/8254 Programmable Interval Timer (PIT)
        pub struct GenericPitDeps;

        feature_gated! {
            feature = "dev_hyperv_vga";

            /// Hyper-V specific VGA graphics card
            pub struct HyperVVgaDeps {
                /// `vmotherboard` bus identifier
                pub attached_to: BusIdPci,
                /// Interface to map SVGABIOS.bin into memory (or None, if that's
                /// handled externally, by the platform itself)
                pub rom: Option<Box<dyn guestmem::MapRom>>,
            }
        }

        /// Generic Dual 8259 Programmable Interrupt Controllers  (PIC)
        pub struct GenericPicDeps {}

        /// Generic IO Advanced Programmable Interrupt Controller (IOAPIC)
        pub struct GenericIoApicDeps {
            /// Number of IO-APIC entries
            pub num_entries: u8,
            /// Trait allowing the IO-APIC device to assert VM interrupts.
            pub routing: Box<dyn ioapic::IoApicRouting>,
        }

        /// Generic MC146818A compatible RTC + CMOS device
        pub struct GenericCmosRtcDeps {
            /// IRQ line to signal RTC device events
            pub irq: u32,
            /// A source of "real time"
            pub time_source: Box<dyn InspectableLocalClock>,
            /// Which CMOS RAM register contains the century register
            pub century_reg_idx: u8,
            /// Initial state of CMOS RAM
            pub initial_cmos: Option<[u8; 256]>,
        }

        /// PIIX4 "flavored" MC146818A compatible RTC + CMOS device
        pub struct Piix4CmosRtcDeps {
            /// A source of "real time"
            pub time_source: Box<dyn InspectableLocalClock>,
            /// Initial state of CMOS RAM
            pub initial_cmos: Option<[u8; 256]>,
            /// Whether enlightened interrupts are enabled. Needed when
            /// advertised by ACPI WAET table.
            pub enlightened_interrupts: bool,
        }

        /// Hyper-V specific ACPI-compatible battery device
        pub struct HyperVBatteryDeps {
            /// Base MMIO address for the battery device
            pub base_addr: u64,
            /// Whether to use gpe0 for battery status updates
            pub use_gpe0: bool,
            /// The line interrupt number to use for battery status updates
            pub line_interrupt_no: u32,
            /// Channel to receive updated battery state
            pub battery_status_recv: mesh::Receiver<HostBatteryUpdate>,
        }

        /// Hyper-V specific Guest Watchdog device
        pub struct HyperVGuestWatchdogDeps {
            /// Port io address of the device's register region
            pub port_base: u16,
            /// Device-specific functions the platform must provide in order to
            /// use this device.
            pub watchdog_platform: Box<dyn watchdog_core::platform::WatchdogPlatform>,
        }

        /// Hyper-V specific UEFI Helper Device
        pub struct HyperVFirmwarePcat {
            /// Bundle of static configuration required by the PCAT BIOS
            /// helper device
            pub config: firmware_pcat::config::PcatBiosConfig,
            /// Interface to log PCAT BIOS events
            pub logger: Box<dyn firmware_pcat::PcatLogger>,
            /// Channel to receive updated generation ID values
            pub generation_id_recv: mesh::Receiver<[u8; 16]>,
            /// Interface to map VMBIOS.bin into memory (or None, if that's
            /// handled externally, by the platform itself)
            pub rom: Option<Box<dyn guestmem::MapRom>>,
            /// Trigger the partition to replay the initially-set MTRRs across
            /// all VPs.
            pub replay_mtrrs: Box<dyn Send + FnMut()>,
        }

        /// Hyper-V specific UEFI Helper Device
        pub struct HyperVFirmwareUefi {
            /// Bundle of static configuration required by the Hyper-V UEFI
            /// helper device
            pub config: firmware_uefi::UefiConfig,
            /// Interface to log UEFI BIOS events
            pub logger: Box<dyn firmware_uefi::platform::logger::UefiLogger>,
            /// Interface for storing/retrieving UEFI NVRAM variables
            pub nvram_storage: Box<dyn uefi_nvram_storage::VmmNvramStorage>,
            /// Channel to receive updated generation ID values
            pub generation_id_recv: mesh::Receiver<[u8; 16]>,
            /// Device-specific functions the platform must provide in order
            /// to use the UEFI watchdog device.
            pub watchdog_platform: Box<dyn watchdog_core::platform::WatchdogPlatform>,
            /// Channel receiver for watchdog timeout notifications.
            pub watchdog_recv: mesh::Receiver<()>,
            /// Interface to revoke VSM on `ExitBootServices()` if requested
            /// by the guest.
            pub vsm_config: Option<Box<dyn firmware_uefi::platform::nvram::VsmConfig>>,
            /// Time source
            pub time_source: Box<dyn InspectableLocalClock>,
        }

        /// Hyper-V specific framebuffer device
        // TODO: this doesn't really belong in base_chipset... it's less-so a
        // device, and more a bit of "infrastructure" that supports other
        // video devices.
        #[expect(missing_docs)] // see TODO above
        pub struct HyperVFramebufferDeps {
            pub fb_mapper: Box<dyn guestmem::MemoryMapper>,
            pub fb: Framebuffer,
            pub vtl2_framebuffer_gpa_base: Option<u64>,
        }

        feature_gated! {
            feature = "dev_underhill_vga_proxy";

            /// Underhill specific VGA proxy device
            pub struct UnderhillVgaProxyDeps {
                /// `vmotherboard` bus identifier
                pub attached_to: BusIdPci,
                /// PCI proxy callbacks
                pub pci_cfg_proxy: Arc<dyn vga_proxy::ProxyVgaPciCfgAccess>,
                /// Host IO hotpath registration object
                pub register_host_io_fastpath: Box<dyn vga_proxy::RegisterHostIoPortFastPath>,
            }
        }
    }
}
