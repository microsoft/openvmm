// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for a serial Pl011 UART chipset device.

use crate::SerialPl011;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_device_resources::IRQ_LINE_SET;
use serial_core::resources::ResolveSerialBackendParams;
use serial_pl011_resources::SerialPl011DeviceHandle;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

/// The resource resolver for [`SerialPl011`].
pub struct SerialPl011Resolver;

declare_static_async_resolver! {
    SerialPl011Resolver,
    (ChipsetDeviceHandleKind, SerialPl011DeviceHandle),
}

/// An error resolving a [`SerialPl011DeviceHandle`].
#[expect(missing_docs)]
#[derive(Debug, Error)]
pub enum ResolvePl011Error {
    #[error("failed to resolve io backend")]
    ResolveBackend(#[source] ResolveError),
    #[error("failed to configure serial device")]
    Configuration(#[source] super::ConfigurationError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, SerialPl011DeviceHandle>
    for SerialPl011Resolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolvePl011Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SerialPl011DeviceHandle,
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
            .map_err(ResolvePl011Error::ResolveBackend)?;

        let interrupt = input
            .configure
            .new_line(IRQ_LINE_SET, "interrupt", resource.irq);

        let device = SerialPl011::new(
            input.device_name.to_string(),
            resource.base,
            interrupt,
            io.0.into_io(),
        )
        .map_err(ResolvePl011Error::Configuration)?;

        Ok(device.into())
    }
}
