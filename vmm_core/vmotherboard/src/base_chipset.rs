// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A flexible chipset builder that pre-populates a [`Chipset`](super::Chipset)
//! with a customizable configuration of semi-standardized device.

use crate::ChipsetDeviceHandle;
use crate::LegacyPciChipsetDeviceHandle;
use crate::PowerEvent;
use crate::chipset::ChipsetBuilder;
use crate::chipset::backing::arc_mutex::device::AddDeviceError;
use crate::chipset::backing::arc_mutex::services::ArcMutexChipsetServices;
use chipset::*;
use chipset_device::ChipsetDevice;
#[cfg(any(
    feature = "dev_generic_isa_floppy",
    feature = "dev_winbond_super_io_and_floppy_full",
    feature = "dev_winbond_super_io_and_floppy_stub"
))]
use chipset_device::isa_dma::IsaDmaController;
use chipset_device_resources::ConfigureChipsetDevice;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use closeable_mutex::CloseableMutex;
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
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::IsaDmaControllerHandleKind;
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
    #[error("failed to resolve ISA DMA controller")]
    ResolveIsaDma(#[source] vm_resource::ResolveError),
    #[error("failed to resolve resource")]
    ResolveResource(#[source] vm_resource::ResolveError),
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
    pci_device_handles: Vec<LegacyPciChipsetDeviceHandle>,
    isa_dma_handle: Option<Resource<IsaDmaControllerHandleKind>>,
    expected_manifest: Option<options::BaseChipsetManifest>,
    fallback_mmio_device: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
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
            pci_device_handles: Vec::new(),
            isa_dma_handle: None,
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

    /// Adds legacy PCI device handles to be resolved and instantiated.
    pub fn with_pci_device_handles(
        mut self,
        mut pci_device_handles: Vec<LegacyPciChipsetDeviceHandle>,
    ) -> Self {
        self.pci_device_handles.append(&mut pci_device_handles);
        self
    }

    /// Sets the ISA DMA controller handle to be resolved and instantiated.
    pub fn with_isa_dma_handle(
        mut self,
        handle: Option<Resource<IsaDmaControllerHandleKind>>,
    ) -> Self {
        self.isa_dma_handle = handle;
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
        fallback_mmio_device: Option<Arc<CloseableMutex<dyn ChipsetDevice>>>,
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
            pci_device_handles,
            isa_dma_handle,
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

        let builder = ChipsetBuilder::new(
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
            deps_generic_isa_floppy,
            deps_generic_pci_bus,
            deps_generic_psp: _, // not actually a device... yet
            deps_hyperv_firmware_pcat,
            deps_hyperv_framebuffer,
            deps_hyperv_ide,
            deps_hyperv_vga,
            deps_piix4_cmos_rtc,
            deps_piix4_pci_bus,
            deps_underhill_vga_proxy,
            deps_winbond_super_io_and_floppy_stub,
            deps_winbond_super_io_and_floppy_full,
        } = devices;

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

        let dma = if let Some(dma_handle) = isa_dma_handle {
            let resolved = resolver
                .resolve(dma_handle, ())
                .await
                .map_err(BaseChipsetBuilderError::ResolveIsaDma)?;
            let dev = builder
                .arc_mutex_device::<dma::DmaController>("dma")
                .add(|_services| resolved.0)?;
            Some(dev)
        } else {
            None
        };

        for device in device_handles {
            let ChipsetDeviceHandle { name, resource } = device;
            builder
                .arc_mutex_device(name.as_ref())
                .try_add_async(async |services| {
                    resolver
                        .resolve(
                            resource,
                            ResolveChipsetDeviceHandleParams {
                                device_name: name.as_ref(),
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
                        .map(|device| device.0)
                })
                .await?;
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
            let resolved = resolver
                .resolve(time_source, ())
                .await
                .map_err(BaseChipsetBuilderError::ResolveResource)?;
            builder.arc_mutex_device("rtc").add(|services| {
                cmos_rtc::Rtc::new(
                    resolved.0,
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
            let resolved = resolver
                .resolve(time_source, ())
                .await
                .map_err(BaseChipsetBuilderError::ResolveResource)?;
            builder.arc_mutex_device("piix4-rtc").add(|services| {
                // hard-coded to IRQ line 8, as per PIIX4 spec
                let rtc_interrupt = services.new_line(IRQ_LINE_SET, "interrupt", 8);
                chipset_legacy::piix4_cmos_rtc::Piix4CmosRtc::new(
                    resolved.0,
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

        for device in pci_device_handles {
            let LegacyPciChipsetDeviceHandle {
                name,
                resource,
                pci_bus_name,
                bdf,
            } = device;

            let (bus, slot, function) = bdf;

            builder
                .arc_mutex_device(name.as_ref())
                .on_pci_bus(crate::BusId::new(pci_bus_name.as_str()))
                .with_pci_addr(bus, slot, function)
                .try_add_async(async |services| {
                    resolver
                        .resolve(
                            resource,
                            ResolveChipsetDeviceHandleParams {
                                device_name: name.as_ref(),
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
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigAccessType;
    use chipset_device::pci::PciConfigAddress;
    use closeable_mutex::CloseableMutex;
    use pci_bus::GenericPciBusDevice;
    use std::sync::Arc;
    use std::sync::Weak;

    /// Wrapper around `Weak<CloseableMutex<dyn ChipsetDevice>>` that implements
    /// [`GenericPciBusDevice`]
    pub struct WeakMutexPciDeviceWrapper(Weak<CloseableMutex<dyn ChipsetDevice>>);

    impl GenericPciBusDevice for WeakMutexPciDeviceWrapper {
        fn pci_cfg_read(
            &mut self,
            offset: u16,
            value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            Some(
                self.0
                    .upgrade()?
                    .lock()
                    .supports_pci()
                    .expect("builder code ensures supports_pci.is_some()")
                    .pci_cfg_read(offset, value),
            )
        }

        fn pci_cfg_write(&mut self, offset: u16, value: ByteEnabledDwordWrite) -> Option<IoResult> {
            Some(
                self.0
                    .upgrade()?
                    .lock()
                    .supports_pci()
                    .expect("builder code ensures supports_pci.is_some()")
                    .pci_cfg_write(offset, value),
            )
        }

        fn pci_cfg_read_with_routing(
            &mut self,
            access_type: PciConfigAccessType,
            address: PciConfigAddress,
            value: ByteEnabledDwordRead<'_>,
        ) -> Option<IoResult> {
            Some(
                self.0
                    .upgrade()?
                    .lock()
                    .supports_pci()
                    .expect("builder code ensures supports_pci.is_some()")
                    .pci_cfg_read_with_routing(access_type, address, value),
            )
        }

        fn pci_cfg_write_with_routing(
            &mut self,
            access_type: PciConfigAccessType,
            address: PciConfigAddress,
            value: ByteEnabledDwordWrite,
        ) -> Option<IoResult> {
            Some(
                self.0
                    .upgrade()?
                    .lock()
                    .supports_pci()
                    .expect("builder code ensures supports_pci.is_some()")
                    .pci_cfg_write_with_routing(access_type, address, value),
            )
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
            port_devfn: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.lock()
                .add_pcie_device(
                    port_devfn,
                    name.clone(),
                    Box::new(WeakMutexPciDeviceWrapper(dev)),
                )
                .map_err(|existing_dev_name| PcieConflict {
                    reason: PcieConflictReason::ExistingDev(existing_dev_name),
                    conflict_dev: name,
                })
        }

        fn downstream_ports(&self) -> Vec<pcie::root::DownstreamPortInfo> {
            self.lock().downstream_ports()
        }

        fn add_rciep(
            &mut self,
            devfn: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.lock()
                .add_rciep(
                    devfn,
                    name.clone(),
                    Box::new(WeakMutexPciDeviceWrapper(dev)),
                )
                .map_err(|existing_dev_name| PcieConflict {
                    reason: PcieConflictReason::ExistingDev(existing_dev_name),
                    conflict_dev: name,
                })
        }
    }

    // wiring to enable using the PCIe switch alongside the Arc+CloseableMutex device infra
    impl RegisterWeakMutexPcie for Arc<CloseableMutex<pcie::switch::GenericPcieSwitch>> {
        fn add_pcie_device(
            &mut self,
            port_devfn: u8,
            name: Arc<str>,
            dev: Weak<CloseableMutex<dyn ChipsetDevice>>,
        ) -> Result<(), PcieConflict> {
            self.lock()
                .add_pcie_device(port_devfn, &name, Box::new(WeakMutexPciDeviceWrapper(dev)))
                .map_err(|err| PcieConflict {
                    reason: PcieConflictReason::ExistingDev(err.to_string().into()),
                    conflict_dev: name,
                })
        }

        fn downstream_ports(&self) -> Vec<pcie::root::DownstreamPortInfo> {
            self.lock().downstream_ports()
        }
    }
}

#[cfg(any(
    feature = "dev_generic_isa_floppy",
    feature = "dev_winbond_super_io_and_floppy_full",
    feature = "dev_winbond_super_io_and_floppy_stub"
))]
pub struct ArcMutexIsaDmaChannel {
    channel_num: u8,
    dma: Arc<CloseableMutex<dma::DmaController>>,
}

#[cfg(any(
    feature = "dev_generic_isa_floppy",
    feature = "dev_winbond_super_io_and_floppy_full",
    feature = "dev_winbond_super_io_and_floppy_stub"
))]
impl ArcMutexIsaDmaChannel {
    pub fn new(dma: Arc<CloseableMutex<dma::DmaController>>, channel_num: u8) -> Self {
        Self { dma, channel_num }
    }
}

#[cfg(any(
    feature = "dev_generic_isa_floppy",
    feature = "dev_winbond_super_io_and_floppy_full",
    feature = "dev_winbond_super_io_and_floppy_stub"
))]
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
            generic_isa_floppy:          dev::GenericIsaFloppyDeps,
            generic_pci_bus:             dev::GenericPciBusDeps,
            generic_psp:                 dev::GenericPspDeps,

            hyperv_firmware_pcat:        dev::HyperVFirmwarePcat,
            hyperv_framebuffer:          dev::HyperVFramebufferDeps,
            hyperv_ide:                  dev::HyperVIdeDeps,
            hyperv_vga:                  dev::HyperVVgaDeps,

            piix4_cmos_rtc:              dev::Piix4CmosRtcDeps,
            piix4_pci_bus:               dev::Piix4PciBusDeps,

            underhill_vga_proxy:         dev::UnderhillVgaProxyDeps,

            winbond_super_io_and_floppy_stub: dev::WinbondSuperIoAndFloppyStubDeps,
            winbond_super_io_and_floppy_full: dev::WinbondSuperIoAndFloppyFullDeps,
        }
    }

    /// Derived capabilities for the configured chipset devices.
    #[derive(MeshPayload, Debug, Copy, Clone)]
    pub struct VmChipsetCapabilities {
        /// Whether the VM exposes an IOAPIC.
        pub with_ioapic: bool,
        /// Whether the VM exposes a legacy PIC.
        pub with_pic: bool,
        /// Whether the VM exposes a PIT.
        pub with_pit: bool,
        /// Whether the VM exposes a generic ISA DMA controller.
        pub with_generic_isa_dma: bool,
        /// Whether the VM exposes a PSP.
        pub with_psp: bool,
        /// Whether the VM exposes the Hyper-V guest watchdog device.
        pub with_guest_watchdog: bool,
        /// Global system interrupt (GSI) for the ACPI power button, if present.
        pub acpi_power_button_gsi: Option<u32>,
        /// Whether the VM exposes an i440BX Host-PCI Bridge (Gen1 legacy PCI bus).
        pub with_i440bx_host_pci_bridge: bool,
        /// COM1 through COM4 ports to describe in the device tree. Device
        /// attachment does not depend on these values.
        pub dt_com_ports: [bool; 4],
        /// Whether to describe both ARM64 PL011 UARTs in the device tree.
        pub dt_pl011_uarts: bool,
    }

    /// Device specific dependencies
    pub mod dev {
        use super::*;
        use crate::BusIdPci;

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

        /// Generic MC146818A compatible RTC + CMOS device
        pub struct GenericCmosRtcDeps {
            /// IRQ line to signal RTC device events
            pub irq: u32,
            /// A time source resource, resolved at device build time.
            pub time_source: Resource<chipset_resources::CmosRtcTimeSourceHandleKind>,
            /// Which CMOS RAM register contains the century register
            pub century_reg_idx: u8,
            /// Initial state of CMOS RAM
            pub initial_cmos: Option<[u8; 256]>,
        }

        /// PIIX4 "flavored" MC146818A compatible RTC + CMOS device
        pub struct Piix4CmosRtcDeps {
            /// A time source resource, resolved at device build time.
            pub time_source: Resource<chipset_resources::CmosRtcTimeSourceHandleKind>,
            /// Initial state of CMOS RAM
            pub initial_cmos: Option<[u8; 256]>,
            /// Whether enlightened interrupts are enabled. Needed when
            /// advertised by ACPI WAET table.
            pub enlightened_interrupts: bool,
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

#[cfg(test)]
mod tests {
    use super::BaseChipsetBuilder;
    use super::BaseChipsetBuilderError;
    use super::BaseChipsetBuilderOutput;
    use super::options::BaseChipsetDevices;
    use super::options::BaseChipsetFoundation;
    use super::options::BaseChipsetManifest;
    use super::options::dev;
    use crate::BusIdPci;
    use crate::ChipsetDeviceHandle;
    use crate::DebugEventHandler;
    use crate::LegacyPciChipsetDeviceHandle;
    use crate::PowerEvent;
    use crate::PowerEventHandler;
    use crate::chipset::Chipset;
    use crate::chipset::tests::Fixture;
    use crate::chipset::tests::Pci;
    use crate::chipset::tests::Shared;
    use crate::chipset::tests::TestDevice;
    use crate::chipset::tests::access;
    use chipset_device::ChipsetDevice;
    use chipset_device_resources::BSP_LINT_LINE_SET;
    use chipset_device_resources::IRQ_LINE_SET;
    use closeable_mutex::CloseableMutex;
    use guestmem::GuestMemory;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vm_resource::IntoResource;
    use vm_resource::ResourceResolver;
    use vmcore::line_interrupt::LineSetTarget;

    struct PanicOnDebugBreak;

    impl DebugEventHandler for PanicOnDebugBreak {
        fn on_debug_break(&self, _: Option<u32>) {
            panic!("unexpected debug break");
        }
    }

    #[derive(Default)]
    struct RecordPower(Mutex<Vec<&'static str>>);

    impl PowerEventHandler for RecordPower {
        fn on_power_event(&self, evt: PowerEvent) {
            self.0.lock().push(match evt {
                PowerEvent::PowerOff => "power-off",
                PowerEvent::Reset => "reset",
                PowerEvent::Hibernate => "hibernate",
            });
        }
    }

    #[derive(Default)]
    struct RecordLines(Mutex<Vec<(u32, bool)>>);

    impl LineSetTarget for RecordLines {
        fn set_irq(&self, vector: u32, high: bool) {
            self.0.lock().push((vector, high));
        }
    }

    async fn base_chipset<'a>(
        fixture: &'a Fixture,
        power: &Arc<RecordPower>,
        devices: BaseChipsetDevices,
        resolver: &ResourceResolver,
        configure: impl FnOnce(BaseChipsetBuilder<'a>) -> BaseChipsetBuilder<'a>,
    ) -> Result<BaseChipsetBuilderOutput<'a>, BaseChipsetBuilderError> {
        let foundation = BaseChipsetFoundation {
            is_restoring: false,
            untrusted_dma_memory: GuestMemory::empty(),
            trusted_vtl0_dma_memory: GuestMemory::empty(),
            power_event_handler: power.clone(),
            debug_event_handler: Arc::new(PanicOnDebugBreak),
            vmtime: &fixture.time,
            vmtime_unit: fixture.time_unit.handle(),
            doorbell_registration: None,
        };
        configure(BaseChipsetBuilder::new(foundation, devices))
            .build(&fixture.source, &fixture.units, resolver)
            .await
    }

    fn generic_pci_bus() -> BaseChipsetDevices {
        BaseChipsetDevices {
            deps_generic_pci_bus: Some(dev::GenericPciBusDeps {
                bus_id: BusIdPci::new("pci0"),
                pio_addr: 0xcf8,
                pio_data: 0xcfc,
            }),
            ..BaseChipsetDevices::empty()
        }
    }

    fn cfg_address(bdf: (u8, u8, u8), offset: u8) -> [u8; 4] {
        let (bus, device, function) = bdf;
        (0x8000_0000
            | (u32::from(bus) << 16)
            | (u32::from(device) << 11)
            | (u32::from(function) << 8)
            | u32::from(offset))
        .to_le_bytes()
    }

    async fn cfg_read(chipset: &Chipset, bdf: (u8, u8, u8), offset: u8) -> u32 {
        chipset.io_write(0, 0xcf8, &cfg_address(bdf, offset)).await;
        let mut data = [0; 4];
        chipset.io_read(0, 0xcfc, &mut data).await;
        u32::from_le_bytes(data)
    }

    async fn cfg_write(chipset: &Chipset, bdf: (u8, u8, u8), offset: u8, value: u32) {
        chipset.io_write(0, 0xcf8, &cfg_address(bdf, offset)).await;
        chipset.io_write(0, 0xcfc, &value.to_le_bytes()).await;
    }

    async fn port_read(chipset: &Chipset, port: u16) -> u8 {
        let mut data = [0];
        chipset.io_read(0, port, &mut data).await;
        data[0]
    }

    async fn port_write(chipset: &Chipset, port: u16, value: u8) {
        chipset.io_write(0, port, &[value]).await;
    }

    // Test: A base chipset with a generic PCI bus at 0xcf8/0xcfc, a PCI device
    //       at 00:03.0, and one that suggests 00:03.0 but is placed at 00:05.0,
    //       accessed through the config address/data ports; then two devices at
    //       the same address.
    // Expected: Each address returns its device's data, an empty slot reads all
    //       ones, writes reach the device, the manifest check passes, and the
    //       duplicate address makes build fail naming both devices.
    #[async_test]
    async fn generic_pci_bus_routes_config_space(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let resolver = ResourceResolver::new();
        let pci_device = |name, fill| TestDevice {
            pci: Pci::SuggestedBdf(0, 3, 0),
            read_fill: fill,
            ..TestDevice::new(name, &shared)
        };

        {
            let output = base_chipset(&fixture, &power, generic_pci_bus(), &resolver, |b| {
                b.with_expected_manifest(BaseChipsetManifest {
                    with_generic_pci_bus: true,
                    ..BaseChipsetManifest::empty()
                })
            })
            .await
            .unwrap();
            let builder = output.chipset_builder;
            builder
                .arc_mutex_device("nic")
                .on_pci_bus(BusIdPci::new("pci0"))
                .add(|_| pci_device("nic", 0x5a))
                .unwrap();
            // Suggests 00:03.0 too, but the explicit address wins.
            builder
                .arc_mutex_device("moved")
                .on_pci_bus(BusIdPci::new("pci0"))
                .with_pci_addr(0, 5, 0)
                .add(|_| pci_device("moved", 0x6b))
                .unwrap();
            let (chipset, _devices) = builder.build().unwrap();

            assert_eq!(cfg_read(&chipset, (0, 3, 0), 0).await, 0x5a5a_5a5a);
            assert_eq!(cfg_read(&chipset, (0, 5, 0), 0).await, 0x6b6b_6b6b);
            assert_eq!(cfg_read(&chipset, (0, 4, 0), 0).await, !0);
            cfg_write(&chipset, (0, 3, 0), 0x10, 0xfebf_0000).await;
            assert_eq!(
                shared.lock().accesses,
                [access("nic", "pci", 0x10, &0xfebf_0000u32.to_le_bytes())]
            );
        }

        let output = base_chipset(&fixture, &power, generic_pci_bus(), &resolver, |b| b)
            .await
            .unwrap();
        let builder = output.chipset_builder;
        for name in ["first", "second"] {
            builder
                .arc_mutex_device(name)
                .on_pci_bus(BusIdPci::new("pci0"))
                .add(|_| pci_device(name, 0))
                .unwrap();
        }
        let err = match builder.build() {
            Ok(_) => panic!("build unexpectedly succeeded"),
            Err(err) => format!("{:#}", anyhow::Error::from(err)),
        };
        assert!(
            err.contains("cannot attach second to 00:03:0, already occupied by first"),
            "{err}"
        );
    }

    // Test: A Gen1 base chipset with the PIIX4 PCI bus, the Hyper-V IDE
    //       controller, and the PIIX4 USB UHCI stub added as a legacy PCI
    //       device handle at 00:07.2; guest writes to the PIIX4 reset register
    //       at 0xcf9; then two handles at the same address.
    // Expected: IDE answers at 00:07.1 with the PIIX4 IDE IDs, the stub answers
    //       at 00:07.2 with its documented register values, only writes with a
    //       reset bit set raise a reset power event, and the duplicate address
    //       fails the build.
    #[async_test]
    async fn gen1_piix4_pci_bus_resets_and_hosts_legacy_pci_devices(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let mut resolver = ResourceResolver::new();
        resolver.add_resolver(chipset_legacy::piix4_uhci::resolver::Piix4PciUsbUhciStubResolver);
        let devices = || BaseChipsetDevices {
            deps_piix4_pci_bus: Some(dev::Piix4PciBusDeps {
                bus_id: BusIdPci::new("piix4"),
            }),
            deps_hyperv_ide: Some(dev::HyperVIdeDeps {
                attached_to: BusIdPci::new("piix4"),
                primary_channel_drives: [None, None],
                secondary_channel_drives: [None, None],
            }),
            ..BaseChipsetDevices::empty()
        };
        let uhci = |name: &str| LegacyPciChipsetDeviceHandle {
            name: name.into(),
            resource: chipset_resources::piix4_uhci::Piix4PciUsbUhciStubDeviceHandle
                .into_resource(),
            pci_bus_name: "piix4".into(),
            bdf: (0, 7, 2),
        };

        {
            let output = base_chipset(&fixture, &power, devices(), &resolver, |b| {
                b.with_pci_device_handles(vec![uhci("uhci")])
            })
            .await
            .unwrap();
            let (chipset, _devices) = output.chipset_builder.build().unwrap();

            assert_eq!(cfg_read(&chipset, (0, 7, 1), 0).await, 0x7111_8086);
            // Status/command, and interrupt pin D with no line assigned.
            assert_eq!(cfg_read(&chipset, (0, 7, 2), 0x04).await, 0x0280_0000);
            assert_eq!(cfg_read(&chipset, (0, 7, 2), 0x3c).await, 0x04ff);
            assert_eq!(cfg_read(&chipset, (0, 7, 3), 0x04).await, !0);

            port_write(&chipset, 0xcf9, 0x01).await;
            assert!(power.0.lock().is_empty());
            port_write(&chipset, 0xcf9, 0x06).await;
            assert_eq!(*power.0.lock(), ["reset"]);
        }

        let output = base_chipset(&fixture, &power, devices(), &resolver, |b| {
            b.with_pci_device_handles(vec![uhci("uhci"), uhci("uhci2")])
        })
        .await
        .unwrap();
        let err = match output.chipset_builder.build() {
            Ok(_) => panic!("build unexpectedly succeeded"),
            Err(err) => format!("{:#}", anyhow::Error::from(err)),
        };
        assert!(
            err.contains("cannot attach uhci2 to 00:07:2, already occupied by uhci"),
            "{err}"
        );
    }

    // Test: A base chipset configured with a fallback MMIO device and tracing
    //       options, accessed at an unclaimed MMIO address and an unclaimed
    //       port.
    // Expected: The MMIO access reaches the fallback device, and the port
    //       access still reads all ones.
    #[async_test]
    async fn base_chipset_forwards_unclaimed_mmio_to_fallback(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let fallback: Arc<CloseableMutex<dyn ChipsetDevice>> =
            Arc::new(CloseableMutex::new(TestDevice {
                read_fill: 0xfb,
                ..TestDevice::new("fallback", &shared)
            }));
        let output = base_chipset(
            &fixture,
            &power,
            BaseChipsetDevices::empty(),
            &ResourceResolver::new(),
            |b| {
                b.with_fallback_mmio_device(Some(fallback))
                    .with_trace_unknown_mmio(false)
                    .with_trace_unknown_pio(true)
            },
        )
        .await
        .unwrap();
        let (chipset, _devices) = output.chipset_builder.build().unwrap();

        let mut data = [0; 4];
        chipset.mmio_read(0, 0x8000, &mut data).await;
        assert_eq!(data, [0xfb; 4]);
        assert_eq!(port_read(&chipset, 0x80).await, !0);
        assert_eq!(
            shared.lock().accesses,
            [access("fallback", "mmio", 0x8000, &[])]
        );
    }

    fn rtc_time_source() -> vm_resource::Resource<chipset_resources::CmosRtcTimeSourceHandleKind> {
        chipset_resources::cmos_rtc_time_source::SystemTimeClockHandle {
            delta_milliseconds: 0,
        }
        .into_resource()
    }

    fn rtc_resolver() -> ResourceResolver {
        let mut resolver = ResourceResolver::new();
        resolver.add_resolver(chipset_resources::cmos_rtc_time_source::SystemTimeClockResolver);
        resolver
    }

    /// Initial CMOS contents with a marker byte in general-purpose RAM.
    const CMOS_MARKER: u8 = 0xab;

    fn initial_cmos() -> [u8; 256] {
        let mut cmos = [0; 256];
        cmos[0x40] = CMOS_MARKER;
        cmos
    }

    /// Checks that CMOS RAM starts with `initial_cmos()` and keeps a write,
    /// using the index/data ports at 0x70/0x71.
    async fn assert_cmos_ram_works(chipset: &Chipset) {
        port_write(chipset, 0x70, 0x40).await;
        assert_eq!(port_read(chipset, 0x71).await, CMOS_MARKER);
        port_write(chipset, 0x70, 0x41).await;
        port_write(chipset, 0x71, 0x5c).await;
        port_write(chipset, 0x70, 0x41).await;
        assert_eq!(port_read(chipset, 0x71).await, 0x5c);
    }

    // Test: Build the generic CMOS RTC with initial CMOS contents, read and
    //       write CMOS RAM through ports 0x70/0x71, then build it without a
    //       time source resolver.
    // Expected: CMOS RAM starts with the supplied contents and keeps guest
    //       writes; a missing time source fails the build.
    #[async_test]
    async fn generic_cmos_rtc_exposes_cmos_ram(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let devices = || BaseChipsetDevices {
            deps_generic_cmos_rtc: Some(dev::GenericCmosRtcDeps {
                irq: 8,
                time_source: rtc_time_source(),
                century_reg_idx: 0x32,
                initial_cmos: Some(initial_cmos()),
            }),
            ..BaseChipsetDevices::empty()
        };

        {
            let output = base_chipset(&fixture, &power, devices(), &rtc_resolver(), |b| b)
                .await
                .unwrap();
            let (chipset, _devices) = output.chipset_builder.build().unwrap();
            assert_cmos_ram_works(&chipset).await;
        }

        let err = base_chipset(&fixture, &power, devices(), &ResourceResolver::new(), |b| b)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(err, BaseChipsetBuilderError::ResolveResource(_)),
            "{err}"
        );
    }

    // Test: Build the Gen1 (PIIX4) CMOS RTC with initial CMOS contents, read
    //       and write CMOS RAM through ports 0x70/0x71, then build it without a
    //       time source resolver.
    // Expected: CMOS RAM starts with the supplied contents and keeps guest
    //       writes; a missing time source fails the build.
    #[async_test]
    async fn gen1_piix4_cmos_rtc_exposes_cmos_ram(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let devices = || BaseChipsetDevices {
            deps_piix4_cmos_rtc: Some(dev::Piix4CmosRtcDeps {
                time_source: rtc_time_source(),
                initial_cmos: Some(initial_cmos()),
                enlightened_interrupts: false,
            }),
            ..BaseChipsetDevices::empty()
        };

        {
            let output = base_chipset(&fixture, &power, devices(), &rtc_resolver(), |b| b)
                .await
                .unwrap();
            let (chipset, _devices) = output.chipset_builder.build().unwrap();
            assert_cmos_ram_works(&chipset).await;
        }

        let err = base_chipset(&fixture, &power, devices(), &ResourceResolver::new(), |b| b)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(err, BaseChipsetBuilderError::ResolveResource(_)),
            "{err}"
        );
    }

    // Test: Build a Gen1 base chipset with an ISA DMA controller handle,
    //       program channel 2's address and page registers through their ports,
    //       and build again without a DMA resolver.
    // Expected: The registers read back what the guest wrote, and the missing
    //       resolver fails the build with the ISA DMA error.
    #[async_test]
    async fn gen1_isa_dma_controller_is_resolved_and_programmable(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let mut resolver = ResourceResolver::new();
        resolver.add_resolver(chipset::dma::resolver::GenericIsaDmaResolver);
        let dma = || Some(chipset_resources::isa_dma::GenericIsaDmaDeviceHandle.into_resource());

        {
            let output = base_chipset(
                &fixture,
                &power,
                BaseChipsetDevices::empty(),
                &resolver,
                |b| b.with_isa_dma_handle(dma()),
            )
            .await
            .unwrap();
            let (chipset, _devices) = output.chipset_builder.build().unwrap();

            // Clear the byte flip-flop, write the 16-bit address low then high.
            port_write(&chipset, 0x0c, 0).await;
            port_write(&chipset, 0x04, 0x34).await;
            port_write(&chipset, 0x04, 0x12).await;
            port_write(&chipset, 0x0c, 0).await;
            assert_eq!(port_read(&chipset, 0x04).await, 0x34);
            assert_eq!(port_read(&chipset, 0x04).await, 0x12);
            port_write(&chipset, 0x81, 0x05).await;
            assert_eq!(port_read(&chipset, 0x81).await, 0x05);
        }

        let err = base_chipset(
            &fixture,
            &power,
            BaseChipsetDevices::empty(),
            &ResourceResolver::new(),
            |b| b.with_isa_dma_handle(dma()),
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(err, BaseChipsetBuilderError::ResolveIsaDma(_)),
            "{err}"
        );
    }

    // Test: Build each Gen1 floppy controller variant (generic ISA, full
    //       Winbond super I/O, stub Winbond super I/O) with and without an ISA
    //       DMA controller.
    // Expected: Each builds when DMA is available and fails with
    //       `NoDmaForFloppy` otherwise.
    #[cfg(any(
        feature = "dev_generic_isa_floppy",
        feature = "dev_winbond_super_io_and_floppy_full",
        feature = "dev_winbond_super_io_and_floppy_stub"
    ))]
    #[async_test]
    async fn gen1_floppy_controllers_require_isa_dma(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let mut resolver = ResourceResolver::new();
        resolver.add_resolver(chipset::dma::resolver::GenericIsaDmaResolver);
        let variants: &[fn() -> BaseChipsetDevices] = &[
            #[cfg(feature = "dev_generic_isa_floppy")]
            || BaseChipsetDevices {
                deps_generic_isa_floppy: Some(dev::GenericIsaFloppyDeps {
                    irq: 6,
                    dma_channel: 2,
                    pio_base: 0x3f0,
                    drives: floppy::DriveRibbon::None,
                }),
                ..BaseChipsetDevices::empty()
            },
            #[cfg(feature = "dev_winbond_super_io_and_floppy_full")]
            || BaseChipsetDevices {
                deps_winbond_super_io_and_floppy_full: Some(dev::WinbondSuperIoAndFloppyFullDeps {
                    primary_disk_drive: floppy::DriveRibbon::None,
                    secondary_disk_drive: floppy::DriveRibbon::None,
                }),
                ..BaseChipsetDevices::empty()
            },
            #[cfg(feature = "dev_winbond_super_io_and_floppy_stub")]
            || BaseChipsetDevices {
                deps_winbond_super_io_and_floppy_stub: Some(dev::WinbondSuperIoAndFloppyStubDeps),
                ..BaseChipsetDevices::empty()
            },
        ];

        for devices in variants {
            let err = base_chipset(&fixture, &power, devices(), &resolver, |b| b)
                .await
                .err()
                .unwrap();
            assert!(
                matches!(err, BaseChipsetBuilderError::NoDmaForFloppy),
                "{err}"
            );

            let output = base_chipset(&fixture, &power, devices(), &resolver, |b| {
                b.with_isa_dma_handle(Some(
                    chipset_resources::isa_dma::GenericIsaDmaDeviceHandle.into_resource(),
                ))
            })
            .await
            .unwrap();
            output.chipset_builder.build().unwrap();
        }
    }

    // Test: A base chipset with a PIC added from a device handle, an external
    //       target on the BSP LINT line set, and a device that raises IRQ 5 and
    //       IRQ 10 after the guest initializes both PICs; then a device handle
    //       that has no resolver.
    // Expected: Each IRQ makes the PIC signal the processor through the
    //       external target and acknowledges as base vector plus IRQ (through
    //       the cascade for IRQ 10); the unresolvable handle fails the build.
    #[async_test]
    async fn irq_lines_reach_the_pic_and_external_targets(driver: DefaultDriver) {
        let fixture = Fixture::new(&driver).await;
        let power = Arc::new(RecordPower::default());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut resolver = ResourceResolver::new();
        resolver.add_resolver(chipset::pic::resolver::PicResolver);
        let pic = || ChipsetDeviceHandle {
            name: "pic".into(),
            resource: chipset_resources::pic::PicDeviceHandle.into_resource(),
        };

        {
            let output = base_chipset(
                &fixture,
                &power,
                BaseChipsetDevices::empty(),
                &resolver,
                |b| b.with_device_handles(vec![pic()]),
            )
            .await
            .unwrap();
            let builder = output.chipset_builder;
            let lint = Arc::new(RecordLines::default());
            builder.add_external_line_target(BSP_LINT_LINE_SET, 0..=0, 0, "test", lint.clone());
            let irq5 = builder
                .arc_mutex_device("irq5")
                .add(|services| TestDevice {
                    line: Some(services.new_line(IRQ_LINE_SET, "test", 5)),
                    ..TestDevice::new("irq5", &shared)
                })
                .unwrap();
            let irq10 = builder
                .arc_mutex_device("irq10")
                .add(|services| TestDevice {
                    line: Some(services.new_line(IRQ_LINE_SET, "test", 10)),
                    ..TestDevice::new("irq10", &shared)
                })
                .unwrap();
            let (chipset, _devices) = builder.build().unwrap();

            // ICW1-ICW4 for the primary (vectors 0x20, secondary on IRQ 2) and
            // secondary (vectors 0x28, cascade identity 2) PICs, then unmask.
            for (port, value) in [
                (0x20, 0x11),
                (0x21, 0x20),
                (0x21, 0x04),
                (0x21, 0x01),
                (0x21, 0x00),
                (0xa0, 0x11),
                (0xa1, 0x28),
                (0xa1, 0x02),
                (0xa1, 0x01),
                (0xa1, 0x00),
            ] {
                port_write(&chipset, port, value).await;
            }
            assert_eq!(chipset.acknowledge_pic_interrupt(), None);

            irq5.lock().line.as_ref().unwrap().set_level(true);
            assert_eq!(lint.0.lock().last(), Some(&(0, true)));
            assert_eq!(chipset.acknowledge_pic_interrupt(), Some(0x25));

            irq10.lock().line.as_ref().unwrap().set_level(true);
            assert_eq!(lint.0.lock().last(), Some(&(0, true)));
            assert_eq!(chipset.acknowledge_pic_interrupt(), Some(0x2a));
        }

        let err = base_chipset(
            &fixture,
            &power,
            BaseChipsetDevices::empty(),
            &ResourceResolver::new(),
            |b| b.with_device_handles(vec![pic()]),
        )
        .await
        .err()
        .unwrap();
        assert!(
            matches!(err, BaseChipsetBuilderError::AddDevice(_)),
            "{err}"
        );
        assert!(err.to_string().contains("pic"), "{err}");
    }
}
