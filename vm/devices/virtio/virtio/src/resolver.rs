// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for virtio device infrastructure.

use crate::PciInterruptModel;
use crate::VirtioPciDevice;
use crate::resolve::VirtioResolveInput;
use async_trait::async_trait;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use thiserror::Error;
use virtio_resources::VirtioPciDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::PciDeviceHandleKind;

declare_static_async_resolver! {
    VirtioPciResolver,
    (PciDeviceHandleKind, VirtioPciDeviceHandle),
}

/// Resolver for [`VirtioPciDeviceHandle`].
pub struct VirtioPciResolver;

#[derive(Debug, Error)]
pub enum ResolveVirtioPciError {
    #[error("failed to resolve virtio device")]
    Virtio(#[source] ResolveError),
    #[error("failed to create PCI device")]
    Pci(#[source] std::io::Error),
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, VirtioPciDeviceHandle> for VirtioPciResolver {
    type Output = ResolvedPciDevice;
    type Error = ResolveVirtioPciError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioPciDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let inner = resolver
            .resolve(
                resource.0,
                VirtioResolveInput {
                    driver_source: input.driver_source,
                    guest_memory: input.guest_memory,
                },
            )
            .await
            .map_err(ResolveVirtioPciError::Virtio)?;

        let device = VirtioPciDevice::new(
            inner.0,
            PciInterruptModel::Msix(input.msi_target),
            input.doorbell_registration,
            input.register_mmio,
            input.shared_mem_mapper,
        )
        .map_err(ResolveVirtioPciError::Pci)?;

        Ok(device.into())
    }
}
