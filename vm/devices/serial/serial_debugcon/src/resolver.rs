// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for a serial debugcon chipset device.

use crate::SerialDebugcon;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use serial_core::resources::ResolveSerialBackendParams;
use serial_debugcon_resources::SerialDebugconDeviceHandle;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

/// The resource resolver for [`SerialDebugcon`].
pub struct SerialDebugconResolver;

declare_static_async_resolver! {
    SerialDebugconResolver,
    (ChipsetDeviceHandleKind, SerialDebugconDeviceHandle),
}

/// An error resolving a [`SerialDebugconDeviceHandle`].
#[allow(missing_docs)]
#[derive(Debug, Error)]
pub enum ResolveDebugconError {
    #[error("failed to resolve io backend")]
    ResolveBackend(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, SerialDebugconDeviceHandle>
    for SerialDebugconResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveDebugconError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SerialDebugconDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let io = resolver
            .resolve(
                resource.io,
                ResolveSerialBackendParams {
                    driver: Box::new(input.task_driver_source.simple()),
                    _async_trait_workaround: &(),
                },
            )
            .await
            .map_err(ResolveDebugconError::ResolveBackend)?;

        let device = SerialDebugcon::new(resource.port, io.0.into_io());
        Ok(device.into())
    }
}
