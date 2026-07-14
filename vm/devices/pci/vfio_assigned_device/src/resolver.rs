// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for VFIO-assigned PCI devices.

use crate::VfioAssignedPciDevice;
use crate::manager::VfioContainerManager;
use crate::manager::VfioManagerClient;
use anyhow::Context as _;
use async_trait::async_trait;
use membacking::DmaMapperClient;
use pal_async::task::Spawn as _;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use std::sync::Arc;
use vfio_assigned_device_resources::VfioCdevDeviceHandle;
use vfio_assigned_device_resources::VfioDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

/// Resource resolver for [`VfioDeviceHandle`].
///
/// Spawns a `VfioContainerManager` task internally and communicates with it
/// via RPC to share VFIO containers across assigned devices.
pub struct VfioDeviceResolver {
    client: VfioManagerClient,
    _task: pal_async::task::Task<()>,
}

impl VfioDeviceResolver {
    /// Create a new resolver, spawning the container manager task.
    ///
    /// The manager registers each new VFIO container with the region manager
    /// so that DMA mappings are kept in sync with the VM's memory map.
    pub fn new(spawner: impl pal_async::task::Spawn, dma_mapper_client: DmaMapperClient) -> Self {
        let mut manager = VfioContainerManager::new(dma_mapper_client);
        let client = manager.client();
        let task = spawner.spawn("vfio-container-mgr", manager.run());
        Self {
            client,
            _task: task,
        }
    }

    /// Returns a handle that can be stored in the VM's inspect tree to
    /// expose the VFIO container/group topology.
    pub fn inspect_handle(&self) -> VfioManagerClient {
        self.client.clone()
    }
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
        let VfioDeviceHandle {
            pci_id,
            group,
            bar_pt,
        } = resource;

        if input.dma_target.software_iommu() {
            anyhow::bail!(
                "VFIO device {pci_id} is behind a software IOMMU that cannot \
                 program the host IOMMU for passthrough DMA. Place the device \
                 on a root complex without a software IOMMU, or wait for \
                 iommufd nested translation support."
            );
        }

        tracing::info!(pci_id, "opening VFIO device");

        // Ask the container manager to prepare (or reuse) a container and
        // group for this device.
        let binding = self
            .client
            .prepare_device(pci_id.clone(), group)
            .await
            .context("VFIO container manager failed")?;

        let memory_mapper = input
            .shared_mem_mapper
            .context("memory mapper is required for VFIO device assignment")?;

        let device = VfioAssignedPciDevice::new(
            binding,
            pci_id,
            input.register_mmio,
            input.dma_target.msi_target(),
            memory_mapper,
            bar_pt,
        )
        .await?;

        Ok(device.into())
    }
}

/// Resource resolver for [`VfioCdevDeviceHandle`] (cdev + iommufd path).
///
/// Spawns a `VfioCdevManager` task internally and communicates with it via RPC
/// to share IOAS contexts across devices referencing the same iommu ID.
pub struct VfioCdevDeviceResolver {
    client: crate::manager::VfioCdevManagerClient,
    _task: pal_async::task::Task<()>,
}

impl VfioCdevDeviceResolver {
    /// Create a new cdev resolver, spawning the cdev dispatcher task.
    pub fn new(
        spawner: impl pal_async::task::Spawn + 'static,
        dma_mapper_client: DmaMapperClient,
    ) -> Self {
        // Arc the spawner so the dispatcher can spawn per-iommu manager tasks.
        let spawner: Arc<dyn pal_async::task::Spawn> = Arc::new(spawner);
        let mut manager = crate::manager::VfioCdevManager::new(spawner.clone(), dma_mapper_client);
        let client = manager.client();
        let task = spawner.spawn("vfio-cdev-dispatch", manager.run());
        Self {
            client,
            _task: task,
        }
    }

    /// Returns a handle for the VM's inspect tree.
    pub fn inspect_handle(&self) -> crate::manager::VfioCdevManagerClient {
        self.client.clone()
    }
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VfioCdevDeviceHandle> for VfioCdevDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VfioCdevDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let VfioCdevDeviceHandle {
            pci_id,
            cdev,
            iommufd,
            iommu_id,
            bar_pt,
        } = resource;

        tracing::info!(pci_id, iommu_id, "opening VFIO cdev device with iommufd");

        let resp = self
            .client
            .prepare_device(crate::manager::CdevPrepareRequest {
                pci_id: pci_id.clone(),
                cdev,
                iommufd,
                iommu_id,
            })
            .await
            .context("VFIO cdev manager failed")?;

        let cdev_binding = crate::manager::VfioCdevBinding::from_response(resp, pci_id.clone());

        let memory_mapper = input
            .shared_mem_mapper
            .context("memory mapper is required for VFIO device assignment")?;

        let device = VfioAssignedPciDevice::from_cdev(
            cdev_binding,
            pci_id,
            input.register_mmio,
            input.dma_target.msi_target(),
            memory_mapper,
            bar_pt,
        )
        .await?;

        Ok(device.into())
    }
}
