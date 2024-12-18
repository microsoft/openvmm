// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for a serial 16550 UART chipset device.

use crate::Serial16550;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_device_resources::IRQ_LINE_SET;
use serial_16550_resources::Serial16550DeviceHandle;
use serial_core::resources::ResolveSerialBackendParams;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

/// The resource resolver for [`Serial16550`].
pub struct Serial16550Resolver;

declare_static_async_resolver! {
    Serial16550Resolver,
    (ChipsetDeviceHandleKind, Serial16550DeviceHandle),
}

/// An error resolving a [`Serial16550DeviceHandle`].
#[expect(missing_docs)]
#[derive(Debug, Error)]
pub enum Resolve16550Error {
    #[error("failed to resolve io backend")]
    ResolveBackend(#[source] ResolveError),
    #[error("failed to configure serial device")]
    Configuration(#[source] super::ConfigurationError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Serial16550DeviceHandle>
    for Serial16550Resolver
{
    type Output = ResolvedChipsetDevice;
    type Error = Resolve16550Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Serial16550DeviceHandle,
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
            .map_err(Resolve16550Error::ResolveBackend)?;

        let interrupt = input
            .configure
            .new_line(IRQ_LINE_SET, "interrupt", resource.irq);

        let device = Serial16550::new(
            input.device_name.to_string(),
            resource.base,
            resource.register_width,
            interrupt,
            io.0.into_io(),
            resource.wait_for_rts,
        )
        .map_err(Resolve16550Error::Configuration)?;

        Ok(device.into())
    }
}
