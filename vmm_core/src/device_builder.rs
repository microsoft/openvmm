// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for resolving and building devices.

use anyhow::Context as _;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use pci_core::msi::MsiInterruptSet;
use pci_core::msi::MsiInterruptTarget;
use std::sync::Arc;
use vm_resource::kind::PciDeviceHandleKind;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vmbus_server::Guid;
use vmbus_server::VmbusServerControl;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
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
    new_virtual_device: impl FnOnce(
        u64,
    ) -> anyhow::Result<(
        Arc<dyn MsiInterruptTarget>,
        Arc<dyn VpciInterruptMapper>,
    )>,
) -> anyhow::Result<()> {
    let device_name = format!("{}:vpci-{instance_id}", resource.id());

    let mut msi_set = MsiInterruptSet::new();

    let device = {
        chipset_builder
            .arc_mutex_device(device_name)
            .with_external_pci()
            .try_add_async(async |services| {
                resolver
                    .resolve(
                        resource,
                        pci_resources::ResolvePciDeviceHandleParams {
                            register_msi: &mut msi_set,
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

    {
        let device_id = (instance_id.data2 as u64) << 16 | (instance_id.data3 as u64 & 0xfff8);
        let vpci_bus_name = format!("vpci:{instance_id}");
        chipset_builder
            .arc_mutex_device(vpci_bus_name)
            .try_add_async(async |services| {
                let (msi_controller, interrupt_mapper) =
                    new_virtual_device(device_id).context("failed to create virtual device")?;

                msi_set.connect(msi_controller.as_ref());

                let bus = vpci::bus::VpciBus::new(
                    driver_source,
                    instance_id,
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
