// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for VFIO-assigned PCI devices.

use crate::VfioAssignedPciDevice;
use crate::manager::VfioContainerManager;
use crate::manager::VfioManagerClient;
use anyhow::Context as _;
use async_trait::async_trait;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use vfio_assigned_device_resources::VfioDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::kind::PciDeviceHandleKind;

/// Resource resolver for [`VfioDeviceHandle`].
///
/// Spawns a [`VfioContainerManager`](crate::manager::VfioContainerManager)
/// task internally and communicates with it via RPC to share VFIO containers
/// across assigned devices.
pub struct VfioDeviceResolver {
    client: VfioManagerClient,
    _task: pal_async::task::Task<()>,
}

impl VfioDeviceResolver {
    /// Create a new resolver, spawning the container manager task.
    ///
    /// The manager lazily initializes DMA mappings on first use, so creating
    /// this is cheap for VMs that have no VFIO devices.
    pub fn new(spawner: impl pal_async::task::Spawn, guest_memory: guestmem::GuestMemory) -> Self {
        let mut manager = VfioContainerManager::new(guest_memory);
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
        let VfioDeviceHandle { pci_id, group } = resource;

        tracing::info!(pci_id, "opening VFIO device");

        // Ask the container manager to prepare (or reuse) a container and
        // group for this device.
        let binding = self
            .client
            .prepare_device(pci_id.clone(), group)
            .await
            .context("VFIO container manager failed")?;

        let driver = input.driver_source.simple();
        let device: vfio_sys::Device = binding
            .group()
            .open_device(&pci_id, &driver)
            .await
            .with_context(|| format!("failed to open VFIO device {pci_id}"))?;

        let irqfd = input
            .irqfd
            .context("partition does not support irqfd (required for VFIO)")?;

        let device = VfioAssignedPciDevice::new(
            binding,
            device,
            pci_id,
            input.register_mmio,
            input.msi_target,
            irqfd,
        )?;

        Ok(device.into())
    }
}
