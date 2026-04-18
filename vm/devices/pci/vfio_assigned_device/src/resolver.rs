// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for VFIO-assigned PCI devices.

use crate::VfioAssignedPciDevice;
use crate::VfioAssignedPciDeviceConfig;
use crate::VfioBarInfo;
use anyhow::Context as _;
use async_trait::async_trait;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use vfio_assigned_device_resources::VfioDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::PciDeviceHandleKind;

/// Resource resolver for [`VfioDeviceHandle`].
pub struct VfioDeviceResolver;

declare_static_async_resolver! {
    VfioDeviceResolver,
    (PciDeviceHandleKind, VfioDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VfioDeviceHandle> for VfioDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VfioDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        use std::path::Path;

        let pci_id = &resource.pci_id;
        let sysfs_path = Path::new("/sys/bus/pci/devices").join(pci_id);

        tracing::info!(pci_id, "opening VFIO device");

        let (container, group) =
            vfio_sys::setup_vfio_container_group(&sysfs_path, vfio_sys::IommuType::Type1v2)
                .with_context(|| format!("failed VFIO setup for {pci_id}"))?;

        // Map guest RAM into the IOMMU for device DMA access. Each
        // shareable region is identity-mapped (IOVA == GPA) so that device
        // DMA addresses match guest physical addresses.
        let sharing = input
            .guest_memory
            .sharing()
            .context("VFIO requires shareable guest memory")?;

        let regions = sharing
            .get_regions()
            .await
            .map_err(|e| anyhow::anyhow!(e))
            .context("failed to get shareable guest memory regions")?;

        let (base_va, va_size) = input
            .guest_memory
            .full_mapping()
            .context("VFIO DMA mapping requires linearly mapped guest memory")?;

        for region in &regions {
            let gpa = region.guest_address;
            let size = region.size;
            let gpa_end = gpa
                .checked_add(size)
                .context("guest memory region overflows u64")?;
            anyhow::ensure!(
                gpa_end <= va_size as u64,
                "guest memory region {:#x}..{:#x} exceeds mapping size {:#x}",
                gpa,
                gpa_end,
                va_size
            );
            let vaddr = base_va as u64 + gpa;
            container.map_dma(gpa, vaddr, size).with_context(|| {
                format!(
                    "failed to map DMA for guest memory region {:#x}..{:#x}",
                    gpa, gpa_end
                )
            })?;
            tracing::debug!(gpa, size, vaddr, "mapped guest RAM for VFIO DMA");
        }

        let driver = input.driver_source.simple();
        let device = group
            .open_device(pci_id, &driver)
            .await
            .with_context(|| format!("failed to open VFIO device {pci_id}"))?;

        let config_info = device
            .region_info(vfio_bindings::bindings::vfio::VFIO_PCI_CONFIG_REGION_INDEX)
            .context("failed to get VFIO config region info")?;

        // Query VFIO region info for each BAR (indices 0-5).
        let mut bar_info = Vec::new();
        for i in 0u32..6 {
            if let Ok(info) = device.region_info(i) {
                if info.size > 0 {
                    bar_info.push(VfioBarInfo {
                        index: i as u8,
                        vfio_offset: info.offset,
                        size: info.size,
                    });
                }
            }
        }

        let irqfd = input
            .irqfd
            .context("partition does not support irqfd (required for VFIO)")?;

        let device = VfioAssignedPciDevice::new(
            VfioAssignedPciDeviceConfig {
                pci_id: pci_id.clone(),
                vfio_device: device,
                config_offset: config_info.offset,
                config_size: config_info.size,
                bar_info,
                vfio_container: container,
                vfio_group: group,
            },
            input.register_mmio,
            input.msi_target,
            irqfd,
        )?;

        Ok(device.into())
    }
}
