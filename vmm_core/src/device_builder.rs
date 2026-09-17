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
    pub async fn remove(self) {
        self.vpci_bus.close().revoke().await;
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
