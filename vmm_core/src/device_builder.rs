// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Functions for resolving and building devices.

use anyhow::Context as _;
use chipset_device_resources::ErasedChipsetDevice;
use guestmem::DoorbellRegistration;
use guestmem::GuestMemory;
use pci_core::msi::MsiConnection;
use pci_core::msi::SignalMsi;
use std::sync::Arc;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;
use vmbus_server::VmbusServerControl;
use vmcore::vm_task::VmTaskDriverSource;
use vmcore::vpci_msi::VpciInterruptMapper;
use vmotherboard::ArcMutexChipsetDeviceBuilder;
use vmotherboard::ChipsetBuilder;

pub use vpci::bus::VpciBusConfig;

/// Common context for resolving and building a PCI device. These parameters
/// are shared across PCIe and VPCI device construction.
pub struct PciDeviceResolveContext<'a> {
    /// The VM's task driver source.
    pub driver_source: &'a VmTaskDriverSource,
    /// The resource resolver.
    pub resolver: &'a ResourceResolver,
    /// The VM's guest memory (possibly SMMU-wrapped for PCIe devices).
    pub guest_memory: &'a GuestMemory,
    /// The device resource to resolve.
    pub resource: Resource<PciDeviceHandleKind>,
    /// An object with which to register doorbell regions.
    pub doorbell_registration: Option<Arc<dyn DoorbellRegistration>>,
    /// An object with which to register shared memory regions.
    pub shared_mem_mapper: Option<&'a dyn guestmem::MemoryMapper>,
    /// Whether the device is behind a software IOMMU (e.g., emulated SMMU)
    /// that cannot program the host IOMMU for passthrough DMA. When `true`,
    /// device assignment backends (e.g., VFIO) must reject the assignment.
    pub software_iommu: bool,
}

/// Resolves a PCI device resource, builds the corresponding device, and builds
/// a VPCI bus to host it.
pub async fn build_vpci_device(
    ctx: PciDeviceResolveContext<'_>,
    vmbus: &VmbusServerControl,
    chipset_builder: &ChipsetBuilder<'_>,
    bus_config: VpciBusConfig,
    new_virtual_device: impl FnOnce(u64) -> anyhow::Result<(Arc<dyn SignalMsi>, VpciInterruptMapper)>,
) -> anyhow::Result<()> {
    let instance_id = bus_config.instance_id;
    let device_name = format!("{}:vpci-{instance_id}", ctx.resource.id());
    let driver_source = ctx.driver_source;

    let device_builder = chipset_builder
        .arc_mutex_device(device_name)
        .with_external_pci();

    let msi_conn = MsiConnection::new(pci_core::bus_range::AssignedBusRange::new(), 0);

    let device = resolve_and_add_pci_device(device_builder, ctx, msi_conn.target()).await?;

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
    msi_target: &pci_core::msi::MsiTarget,
) -> anyhow::Result<()> {
    let dev_name = format!("pcie:{}-{}", port_name, ctx.resource.id());
    let device_builder = chipset_builder
        .arc_mutex_device(dev_name)
        .on_pcie_port(vmotherboard::BusId::new(&port_name));

    resolve_and_add_pci_device(device_builder, ctx, msi_target).await?;

    Ok(())
}

/// Resolves a PCI device resource and adds it to the specified chipset device
/// builder.
pub async fn resolve_and_add_pci_device(
    device_builder: ArcMutexChipsetDeviceBuilder<'_, '_, ErasedChipsetDevice>,
    ctx: PciDeviceResolveContext<'_>,
    msi_target: &pci_core::msi::MsiTarget,
) -> anyhow::Result<Arc<closeable_mutex::CloseableMutex<ErasedChipsetDevice>>> {
    let device = device_builder
        .try_add_async(async |services| {
            ctx.resolver
                .resolve(
                    ctx.resource,
                    pci_resources::ResolvePciDeviceHandleParams {
                        msi_target,
                        register_mmio: &mut services.register_mmio(),
                        driver_source: ctx.driver_source,
                        guest_memory: ctx.guest_memory,
                        doorbell_registration: ctx.doorbell_registration,
                        shared_mem_mapper: ctx.shared_mem_mapper,
                        software_iommu: ctx.software_iommu,
                    },
                )
                .await
                .map(|r| r.0)
        })
        .await?;

    Ok(device)
}
