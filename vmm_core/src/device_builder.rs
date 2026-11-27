// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for resolving and building devices.

use anyhow::Context as _;
use chipset_device_resources::ErasedChipsetDevice;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use pci_core::dma::DmaTarget;
use pci_core::msi::MsiConnection;
use pci_core::msi::SignalMsi;
use state_unit::StateUnits;
use std::sync::Arc;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmbus_server::VmbusServerControl;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ArcMutexChipsetDeviceBuilder;
use vmotherboard::ChipsetBuilder;
use vmotherboard::ChipsetDevices;
use vmotherboard::DynamicDeviceUnit;

pub use vpci::bus::VpciBusConfig;

/// Common context for resolving and building a PCI device. These parameters
/// are shared across PCIe and VPCI device construction.
pub struct PciDeviceResolveContext<'a> {
    /// The VM's task driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// The resource resolver.
    pub resolver: &'a ResourceResolver,
    /// The device resource to resolve.
    pub resource: Resource<PciDeviceHandleKind>,
    /// An object with which to register doorbell regions.
    pub doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    /// An object with which to register shared memory regions.
    pub shared_mem_mapper: Option<&'a dyn guestmem::MemoryMapper>,
}

/// A dynamically created VPCI bus and its underlying PCI device.
pub struct DynamicVpciDevice {
    pci_unit: DynamicDeviceUnit,
    vpci_unit: DynamicDeviceUnit,
    eject: vpci::bus::VpciBusEject,
    vpci_bus: Arc<closeable_mutex::CloseableMutex<vpci::bus::VpciBus>>,
}

impl DynamicVpciDevice {
    /// Requests device ejection and waits for the guest to acknowledge it.
    pub async fn eject(&self) -> anyhow::Result<()> {
        self.eject.eject().await
    }

    /// Removes the VPCI bus before removing its underlying PCI device.
    ///
    /// The bus remains available to VPs and to inspection while its VMBus
    /// channel is revoked.
    pub async fn remove(self) {
        // Release the lock before waiting for the revocation.
        let revoke = self.vpci_bus.lock().revoke();
        revoke.await;
        self.vpci_unit.remove().await;
        self.pci_unit.remove().await;
    }
}

/// Resolves a PCI device resource and dynamically creates a VPCI bus to host it.
pub async fn build_dynamic_vpci_device(
    ctx: PciDeviceResolveContext<'_>,
    vmbus: &VmbusServerControl,
    chipset_devices: &ChipsetDevices,
    state_units: &mut StateUnits,
    bus_config: VpciBusConfig,
    guest_memory: GuestMemory,
    new_virtual_device: impl FnOnce(u64) -> anyhow::Result<(Arc<dyn SignalMsi>, VpciInterruptMapper)>,
) -> anyhow::Result<DynamicVpciDevice> {
    let instance_id = bus_config.instance_id;
    let device_name = format!("{}:vpci-{instance_id}", ctx.resource.id());
    let driver_source = ctx.driver_source;
    let msi_conn = MsiConnection::new();
    let dma_target = DmaTarget::new(
        pci_core::bus_range::AssignedBusRange::new(),
        0,
        guest_memory,
        &msi_conn,
    );

    let (pci_unit, device) = chipset_devices
        .add_dyn_device(
            driver_source,
            state_units,
            device_name,
            async |register_mmio| {
                ctx.resolver
                    .resolve(
                        ctx.resource,
                        pci_resources::ResolvePciDeviceHandleParams {
                            dma_target: &dma_target,
                            register_mmio,
                            driver_source,
                            doorbell_registration: ctx.doorbell_registration,
                            shared_mem_mapper: ctx.shared_mem_mapper,
                        },
                    )
                    .await
                    .map(|r| r.0)
                    .map_err(anyhow::Error::from)
            },
        )
        .await?;

    let device_id = (instance_id.data2 as u64) << 16 | (instance_id.data3 as u64 & 0xfff8);
    let mut pending_offer = None;
    let vpci_unit = chipset_devices
        .add_dyn_device(
            driver_source,
            state_units,
            format!("vpci:{instance_id}"),
            async |register_mmio| {
                let (msi_controller, interrupt_mapper) =
                    new_virtual_device(device_id).context(format!(
                        "failed to create virtual device, device_id {device_id} = {} | {}",
                        instance_id.data2,
                        instance_id.data3 as u64 & 0xfff8
                    ))?;
                msi_conn.connect(msi_controller);
                let (bus, offer) = vpci::bus::VpciBus::new_unoffered(
                    bus_config,
                    device,
                    register_mmio,
                    interrupt_mapper,
                )
                .map_err(anyhow::Error::from)?;
                pending_offer = Some(offer);
                anyhow::Ok(bus)
            },
        )
        .await;

    let (vpci_unit, vpci_bus) = match vpci_unit {
        Ok(device) => device,
        Err(error) => {
            pci_unit.remove().await;
            return Err(error);
        }
    };

    let pending_offer = pending_offer.context("missing deferred VPCI channel offer")?;
    state_units.start_stopped_units().await;
    if let Err(error) = pending_offer
        .offer_registered(&vpci_bus, driver_source, vmbus, state_units.is_running())
        .await
    {
        vpci_unit.remove().await;
        pci_unit.remove().await;
        return Err(error);
    }

    let eject = vpci_bus.lock().eject_control();
    Ok(DynamicVpciDevice {
        pci_unit,
        vpci_unit,
        eject,
        vpci_bus,
    })
}

/// Resolves a PCI device resource, builds the corresponding device, and builds
/// a VPCI bus to host it.
///
/// VPCI devices deliver interrupts through the vmbus [`VpciInterruptMapper`]
/// rather than a PCIe [`MsiTarget`](pci_core::msi::MsiTarget), so this builds a
/// fresh [`DmaTarget`] pairing `guest_memory` with a locally-owned
/// [`MsiConnection`] that is connected to the virtual device's MSI controller.
pub async fn build_vpci_device(
    ctx: PciDeviceResolveContext<'_>,
    vmbus: &VmbusServerControl,
    chipset_builder: &ChipsetBuilder<'_>,
    bus_config: VpciBusConfig,
    guest_memory: GuestMemory,
    new_virtual_device: impl FnOnce(u64) -> anyhow::Result<(Arc<dyn SignalMsi>, VpciInterruptMapper)>,
) -> anyhow::Result<()> {
    let instance_id = bus_config.instance_id;
    let device_name = format!("{}:vpci-{instance_id}", ctx.resource.id());
    let driver_source = ctx.driver_source;

    let device_builder = chipset_builder
        .arc_mutex_device(device_name)
        .with_external_pci();

    let msi_conn = MsiConnection::new();

    let dma_target = DmaTarget::new(
        pci_core::bus_range::AssignedBusRange::new(),
        0,
        guest_memory,
        &msi_conn,
    );
    let device = resolve_and_add_pci_device(device_builder, ctx, &dma_target).await?;

    {
        let device_id = (instance_id.data2 as u64) << 16 | (instance_id.data3 as u64 & 0xfff8);
        let vpci_bus_name = format!("vpci:{instance_id}");
        chipset_builder
            .arc_mutex_device(vpci_bus_name)
            .try_add_async(async |services| {
                let (msi_controller, interrupt_mapper) =
                    new_virtual_device(device_id).context(format!(
                        "failed to create virtual device, device_id {device_id} = {} | {}",
                        instance_id.data2,
                        instance_id.data3 as u64 & 0xfff8
                    ))?;

                msi_conn.connect(msi_controller);

                let bus = vpci::bus::VpciBus::new(
                    driver_source,
                    bus_config,
                    device,
                    &mut services.register_mmio(),
                    vmbus,
                    interrupt_mapper,
                )
                .await?;

                anyhow::Ok(bus)
            })
            .await?;
    }

    Ok(())
}

/// Resolves a PCI device resource, builds the corresponding device, and attaches
/// the device at the specified PCIe port.
pub async fn build_pcie_device(
    ctx: PciDeviceResolveContext<'_>,
    chipset_builder: &ChipsetBuilder<'_>,
    port_name: Arc<str>,
    dma_target: &DmaTarget,
) -> anyhow::Result<()> {
    let dev_name = format!("pcie:{}-{}", port_name, ctx.resource.id());
    let device_builder = chipset_builder
        .arc_mutex_device(dev_name)
        .on_pcie_port(vmotherboard::BusId::new(&port_name));

    resolve_and_add_pci_device(device_builder, ctx, dma_target).await?;

    Ok(())
}

/// Resolves a PCI device resource and adds it to the specified chipset device
/// builder.
pub async fn resolve_and_add_pci_device(
    device_builder: ArcMutexChipsetDeviceBuilder<'_, '_, ErasedChipsetDevice>,
    ctx: PciDeviceResolveContext<'_>,
    dma_target: &DmaTarget,
) -> anyhow::Result<Arc<closeable_mutex::CloseableMutex<ErasedChipsetDevice>>> {
    let device = device_builder
        .try_add_async(async |services| {
            ctx.resolver
                .resolve(
                    ctx.resource,
                    pci_resources::ResolvePciDeviceHandleParams {
                        dma_target,
                        register_mmio: &mut services.register_mmio(),
                        driver_source: ctx.driver_source,
                        doorbell_registration: ctx.doorbell_registration,
                        shared_mem_mapper: ctx.shared_mem_mapper,
                    },
                )
                .await
                .map(|r| r.0)
        })
        .await?;

    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::DynamicVpciDevice;
    use chipset_device::ChipsetDevice;
    use chipset_device::io::IoResult;
    use chipset_device::pci::ByteEnabledDwordRead;
    use chipset_device::pci::ByteEnabledDwordWrite;
    use chipset_device::pci::PciConfigSpace;
    use futures::poll;
    use guestmem::GuestMemory;
    use inspect::InspectMut;
    use pal_async::DefaultDriver;
    use pal_async::async_test;
    use parking_lot::Mutex;
    use state_unit::StateUnits;
    use std::pin::pin;
    use std::sync::Arc;
    use test_with_tracing::test;
    use vm_resource::ResourceResolver;
    use vmbus_channel::bus::OfferInput;
    use vmbus_channel::bus::OfferResources;
    use vmbus_channel::bus::ParentBus;
    use vmcore::device_state::ChangeDeviceState;
    use vmcore::save_restore::NoSavedState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;
    use vmcore::vm_task::SingleDriverBackend;
    use vmcore::vm_task::VmTaskDriverSource;
    use vmcore::vmtime::VmTime;
    use vmcore::vmtime::VmTimeKeeper;
    use vmcore::vpci_msi::MapVpciInterrupt;
    use vmcore::vpci_msi::MsiAddressData;
    use vmcore::vpci_msi::RegisterInterruptError;
    use vmcore::vpci_msi::VpciInterruptMapper;
    use vmcore::vpci_msi::VpciInterruptParameters;
    use vmotherboard::BaseChipsetBuilder;
    use vmotherboard::DebugEventHandler;
    use vmotherboard::PowerEvent;
    use vmotherboard::PowerEventHandler;
    use vmotherboard::options::BaseChipsetDevices;
    use vmotherboard::options::BaseChipsetFoundation;
    use vpci::bus::VpciBus;
    use vpci::bus::VpciBusConfig;

    /// A PCI device without BARs.
    #[derive(InspectMut)]
    #[inspect(skip)]
    struct TestPciDevice;

    impl ChipsetDevice for TestPciDevice {
        fn supports_pci(&mut self) -> Option<&mut dyn PciConfigSpace> {
            Some(self)
        }
    }

    impl PciConfigSpace for TestPciDevice {
        fn pci_cfg_read(&mut self, _offset: u16, mut value: ByteEnabledDwordRead<'_>) -> IoResult {
            value.set(0);
            IoResult::Ok
        }

        fn pci_cfg_write(&mut self, _offset: u16, _value: ByteEnabledDwordWrite) -> IoResult {
            IoResult::Ok
        }
    }

    impl ChangeDeviceState for TestPciDevice {
        fn start(&mut self) {}

        async fn stop(&mut self) {}

        async fn reset(&mut self) {}
    }

    impl SaveRestore for TestPciDevice {
        type SavedState = NoSavedState;

        fn save(&mut self) -> Result<NoSavedState, SaveError> {
            Ok(NoSavedState)
        }

        fn restore(&mut self, NoSavedState: NoSavedState) -> Result<(), RestoreError> {
            Ok(())
        }
    }

    struct NoInterrupts;

    impl MapVpciInterrupt for NoInterrupts {
        async fn register_interrupt(
            &self,
            _vector_count: u32,
            _params: &VpciInterruptParameters<'_>,
        ) -> Result<MsiAddressData, RegisterInterruptError> {
            Err(RegisterInterruptError::new("interrupts are not supported"))
        }

        async fn unregister_interrupt(&self, _address: u64, _data: u32) {}
    }

    struct NoEvents;

    impl PowerEventHandler for NoEvents {
        fn on_power_event(&self, _evt: PowerEvent) {}
    }

    impl DebugEventHandler for NoEvents {
        fn on_debug_break(&self, _vp: Option<u32>) {}
    }

    /// A VMBus that holds each offered channel until the test releases it, so
    /// that the channel's revocation stays pending until then.
    #[derive(Clone, Default)]
    struct HeldChannels(Arc<Mutex<Vec<OfferInput>>>);

    impl HeldChannels {
        fn release(&self) {
            self.0.lock().clear();
        }
    }

    #[async_trait::async_trait]
    impl ParentBus for HeldChannels {
        async fn add_child(&self, request: OfferInput) -> anyhow::Result<OfferResources> {
            self.0.lock().push(request);
            Ok(OfferResources::new(GuestMemory::empty(), None))
        }

        fn clone_bus(&self) -> Box<dyn ParentBus> {
            Box::new(self.clone())
        }

        fn use_event(&self) -> bool {
            false
        }
    }

    #[async_test]
    async fn removal_keeps_the_bus_open_while_its_channel_is_revoked(driver: DefaultDriver) {
        let driver_source = VmTaskDriverSource::new(SingleDriverBackend::new(driver.clone()));
        let units = StateUnits::new();
        let (vmtime_send, _vmtime_recv) = mesh::channel();
        let vmtime_unit = units.add("vmtime").build(vmtime_send).unwrap();
        let vmtime_keeper = VmTimeKeeper::new(&driver, VmTime::from_100ns(0));
        let vmtime = vmtime_keeper.builder().build(&driver).await.unwrap();
        let events = Arc::new(NoEvents);
        let chipset = BaseChipsetBuilder::new(
            BaseChipsetFoundation {
                is_restoring: false,
                untrusted_dma_memory: GuestMemory::empty(),
                trusted_vtl0_dma_memory: GuestMemory::empty(),
                power_event_handler: events.clone(),
                debug_event_handler: events,
                vmtime: &vmtime,
                vmtime_unit: &vmtime_unit,
                doorbell_registration: None,
            },
            BaseChipsetDevices::empty(),
        )
        .build(&driver_source, &units, &ResourceResolver::new())
        .await
        .unwrap();
        let (_chipset, devices) = chipset.chipset_builder.build().unwrap();

        // Assemble the device as `build_dynamic_vpci_device` does, but offer
        // its channel to a VMBus that the test controls.
        let (pci_unit, pci_device) = devices
            .add_dyn_device(&driver_source, &units, "pci", async |_| {
                anyhow::Ok(TestPciDevice)
            })
            .await
            .unwrap();
        let mut pending_offer = None;
        let (vpci_unit, vpci_bus) = devices
            .add_dyn_device(&driver_source, &units, "vpci", async |register_mmio| {
                let (bus, offer) = VpciBus::new_unoffered(
                    VpciBusConfig {
                        instance_id: guid::guid!("6f1bc3e5-07a5-4b4f-9d36-0c5e4ad0f6f1"),
                        vtom: None,
                        vnode: None,
                    },
                    pci_device.clone(),
                    register_mmio,
                    VpciInterruptMapper::new(Arc::new(NoInterrupts)),
                )?;
                pending_offer = Some(offer);
                anyhow::Ok(bus)
            })
            .await
            .unwrap();
        let vmbus = HeldChannels::default();
        pending_offer
            .unwrap()
            .offer_registered(&vpci_bus, &driver_source, &vmbus, false)
            .await
            .unwrap();
        let eject = vpci_bus.lock().eject_control();
        let device = DynamicVpciDevice {
            pci_unit,
            vpci_unit,
            eject,
            vpci_bus: vpci_bus.clone(),
        };

        // Removal revokes the channel and then waits until the VMBus releases
        // it.
        let mut removal = pin!(device.remove());
        assert!(poll!(&mut removal).is_pending());

        // Meanwhile, the bus still serves inspection through its state unit...
        let mut inspection = inspect::inspect("vpci/state", &units);
        inspection.resolve().await;
        assert_eq!(inspection.results().json().to_string(), r#""revoked""#);
        // ...and VP accesses, which chipset dispatch makes only to an open bus.
        assert!(vpci_bus.lock_if_open().is_some());
        assert!(poll!(&mut removal).is_pending());

        vmbus.release();
        removal.await;
        assert!(vpci_bus.lock_if_open().is_none());
        assert!(pci_device.lock_if_open().is_none());
    }
}
