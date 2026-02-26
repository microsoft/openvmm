// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the nvme controller.

use crate::GdmaDevice;
use crate::VportConfig;
use async_trait::async_trait;
use futures::future::try_join_all;
use gdma_resources::GdmaDeviceHandle;
use net_backend::resolve::ResolveEndpointParams;
use pci_resources::ResolvePciDeviceHandleParams;
use pci_resources::ResolvedPciDevice;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::PciDeviceHandleKind;

/// Resource resolver for [`GdmaDeviceHandle`].
pub struct GdmaDeviceResolver;

declare_static_async_resolver! {
    GdmaDeviceResolver,
    (PciDeviceHandleKind, GdmaDeviceHandle),
}

/// Error returned by [`GdmaDeviceResolver`].
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum Error {
    #[error("failed to resolve vport")]
    VportResolve(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<PciDeviceHandleKind, GdmaDeviceHandle> for GdmaDeviceResolver {
    type Output = ResolvedPciDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: GdmaDeviceHandle,
        input: ResolvePciDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let vports = try_join_all(resource.vports.into_iter().map(async |vport| {
            let endpoint = resolver
                .resolve(
                    vport.endpoint,
                    ResolveEndpointParams {
                        mac_address: vport.mac_address,
                    },
                )
                .await
                .map_err(Error::VportResolve)?;

            Ok(VportConfig {
                mac_address: vport.mac_address,
                endpoint: endpoint.0,
            })
        }))
        .await?;

        let device = GdmaDevice::new(
            input.driver_source,
            input.guest_memory.clone(),
            input.msi_target,
            vports,
            input.register_mmio,
            resource.is_firmware_managed,
        );
        Ok(device.into())
    }
}
