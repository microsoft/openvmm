// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for i8042 devices.

use super::I8042Device;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_resources::i8042::I8042DeviceHandle;
use power_resources::PowerRequest;
use power_resources::PowerRequestHandleKind;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

/// A resolver for i8042 devices.
pub struct I8042Resolver;

declare_static_async_resolver! {
    I8042Resolver,
    (ChipsetDeviceHandleKind, I8042DeviceHandle),
}

/// Errors that can occur when resolving an i8042 device.
#[derive(Debug, Error)]
#[allow(missing_docs)]
pub enum ResolveI8042Error {
    #[error("failed to resolve keyboard input")]
    ResolveKeyboardInput(#[source] ResolveError),
    #[error("failed to resolve power request")]
    ResolvePowerRequest(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, I8042DeviceHandle> for I8042Resolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveI8042Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: I8042DeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Hard-coded to IRQ line 1, as per x86 spec.
        let keyboard_interrupt = input.configure.new_line(IRQ_LINE_SET, "keyboard", 1);

        // Hard-coded to IRQ line 12, as per x86 convention.
        let mouse_interrupt = input.configure.new_line(IRQ_LINE_SET, "aux", 12);

        let keyboard_input = resolver
            .resolve(resource.keyboard_input, input.device_name)
            .await
            .map_err(ResolveI8042Error::ResolveKeyboardInput)?;

        let power_request = resolver
            .resolve::<PowerRequestHandleKind, _>(PlatformResource.into_resource(), ())
            .await
            .map_err(ResolveI8042Error::ResolvePowerRequest)?;

        let reset = Box::new(move || {
            power_request.power_request(PowerRequest::Reset);
        });

        Ok(
            I8042Device::new(reset, keyboard_interrupt, mouse_interrupt, keyboard_input.0)
                .await
                .into(),
        )
    }
}
