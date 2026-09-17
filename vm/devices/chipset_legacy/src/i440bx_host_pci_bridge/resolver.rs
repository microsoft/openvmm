// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the i440BX Host-PCI Bridge device.

use super::HostPciBridge;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::i440bx_host_pci_bridge::AdjustGpaRangeHandleKind;
use chipset_resources::i440bx_host_pci_bridge::I440BxHostPciBridgeDeviceHandle;
use chipset_resources::i440bx_host_pci_bridge::ResolvedAdjustGpaRange;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for the i440BX Host-PCI Bridge device.
pub struct I440BxHostPciBridgeResolver;

declare_static_async_resolver! {
    I440BxHostPciBridgeResolver,
    (ChipsetDeviceHandleKind, I440BxHostPciBridgeDeviceHandle),
}

/// Errors that can occur when resolving an i440BX Host-PCI Bridge device.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolveI440BxHostPciBridgeError {
    #[error("failed to resolve adjust_gpa_range platform resource")]
    ResolveAdjustGpaRange(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, I440BxHostPciBridgeDeviceHandle>
    for I440BxHostPciBridgeResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveI440BxHostPciBridgeError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: I440BxHostPciBridgeDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let ResolvedAdjustGpaRange(adjust_gpa_range) = resolver
            .resolve::<AdjustGpaRangeHandleKind, _>(resource.adjust_gpa_range, ())
            .await
            .map_err(ResolveI440BxHostPciBridgeError::ResolveAdjustGpaRange)?;

        Ok(HostPciBridge::new(adjust_gpa_range, input.is_restoring).into())
    }
}
