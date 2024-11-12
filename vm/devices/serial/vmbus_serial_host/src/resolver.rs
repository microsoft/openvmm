// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for vmbus serial devices.

use crate::Port;
use crate::Serial;
use async_trait::async_trait;
use serial_core::resources::ResolveSerialBackendParams;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;
use vmbus_serial_resources::VmbusSerialDeviceHandle;
use vmbus_serial_resources::VmbusSerialPort;

/// Resolver for [`VmbusSerialDeviceHandle`].
pub struct VmbusSerialDeviceResolver;

declare_static_async_resolver!(
    VmbusSerialDeviceResolver,
    (VmbusDeviceHandleKind, VmbusSerialDeviceHandle)
);

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, VmbusSerialDeviceHandle>
    for VmbusSerialDeviceResolver
{
    type Output = ResolvedVmbusDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VmbusSerialDeviceHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let driver = input.driver_source.simple();
        let port = match resource.port {
            VmbusSerialPort::Com1 => Port::Com1,
            VmbusSerialPort::Com2 => Port::Com2,
        };
        let io = resolver
            .resolve(
                resource.backend,
                ResolveSerialBackendParams {
                    driver: Box::new(driver.clone()),
                    _async_trait_workaround: &(),
                },
            )
            .await?
            .0
            .into_io();

        Ok(SimpleDeviceWrapper::new(driver, Serial::new(port, io)).into())
    }
}
