// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for resolving and building devices.

use anyhow::Context as _;
use chipset_device_resources::ErasedChipsetDevice;
use closeable_mutex::CloseableMutex;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use pci_core::msi::MsiConnection;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmbus_server::Guid;
use vmbus_server::VmbusServerControl;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ArcMutexChipsetDeviceBuilder;
use vmotherboard::ChipsetBuilder;

/// Resolves a PCI device resource, builds the corresponding device, and builds
/// a VPCI bus to host it.
pub async fn build_vpci_device(
    driver_source: &VmTaskDriverSource,
    resolver: &ResourceResolver,
    guest_memory: &GuestMemory,
    vmbus: &VmbusServerControl,
    instance_id: Guid,
    resource: Resource<PciDeviceHandleKind>,
    chipset_builder: &mut ChipsetBuilder<'_>,
    doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    mapper: Option<&dyn guestmem::MemoryMapper>,
    new_virtual_device: impl FnOnce(u64) -> anyhow::Result<(Arc<dyn SignalMsi>, VpciInterruptMapper)>,
    vtom: Option<u64>,
) -> anyhow::Result<()> {
    let device_name = format!("{}:vpci-{instance_id}", resource.id());

    let device_builder = chipset_builder
        .arc_mutex_device(device_name)
        .with_external_pci();

    let (device, msi_conn) = resolve_and_add_pci_device(
        device_builder,
        driver_source,
        resolver,
        guest_memory,
        resource,
        doorbell_registration,
        mapper,
    )
    .await?;

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

                msi_conn.connect(0, msi_controller);

                let bus = vpci::bus::VpciBus::new(
                    driver_source,
                    instance_id,
                    device,
                    &mut services.register_mmio(),
                    vmbus,
                    interrupt_mapper,
                    vtom,
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
    chipset_builder: &mut ChipsetBuilder<'_>,
    port_name: Arc<str>,
    driver_source: &VmTaskDriverSource,
    resolver: &ResourceResolver,
    guest_memory: &GuestMemory,
    resource: Resource<PciDeviceHandleKind>,
    doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    mapper: Option<&dyn guestmem::MemoryMapper>,
    interrupt_target: Option<Arc<dyn SignalMsi>>,
) -> anyhow::Result<()> {
    let dev_name = format!("pcie:{}-{}", port_name, resource.id());
    let device_builder = chipset_builder
        .arc_mutex_device(dev_name)
        .on_pcie_port(vmotherboard::BusId::new(&port_name));

    let (_, msi_conn) = resolve_and_add_pci_device(
        device_builder,
        driver_source,
        resolver,
        guest_memory,
        resource,
        doorbell_registration,
        mapper,
    )
    .await?;

    if let Some(target) = interrupt_target {
        msi_conn.connect(0, target);
    }

    Ok(())
}

/// Resolves a PCI device resource and adds it to the specified chipset device builder.
pub async fn resolve_and_add_pci_device(
    device_builder: ArcMutexChipsetDeviceBuilder<'_, '_, ErasedChipsetDevice>,
    driver_source: &VmTaskDriverSource,
    resolver: &ResourceResolver,
    guest_memory: &GuestMemory,
    resource: Resource<PciDeviceHandleKind>,
    doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    mapper: Option<&dyn guestmem::MemoryMapper>,
) -> anyhow::Result<(Arc<CloseableMutex<ErasedChipsetDevice>>, MsiConnection)> {
    let msi_conn = MsiConnection::new(1);

    let device = {
        device_builder
            .try_add_async(async |services| {
                resolver
                    .resolve(
                        resource,
                        pci_resources::ResolvePciDeviceHandleParams {
                            msi_target: msi_conn.target(),
                            register_mmio: &mut services.register_mmio(),
                            driver_source,
                            guest_memory,
                            doorbell_registration,
                            shared_mem_mapper: mapper,
                        },
                    )
                    .await
                    .map(|r| r.0)
            })
            .await?
    };

    Ok((device, msi_conn))
}
