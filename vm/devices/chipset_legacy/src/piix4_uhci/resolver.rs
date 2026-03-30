// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the PIIX4 USB UHCI stub device.

use super::Piix4UsbUhciStub;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::piix4_uhci::Piix4PciUsbUhciStubDeviceHandle;
use std::convert::Infallible;
use vm_resource::AsyncResolveResource;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for the PIIX4 USB UHCI stub device.
pub struct Piix4PciUsbUhciStubResolver;

declare_static_async_resolver! {
    Piix4PciUsbUhciStubResolver,
    (ChipsetDeviceHandleKind, Piix4PciUsbUhciStubDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Piix4PciUsbUhciStubDeviceHandle>
    for Piix4PciUsbUhciStubResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = Infallible;

    async fn resolve(
        &self,
        _resolver: &vm_resource::ResourceResolver,
        _resource: Piix4PciUsbUhciStubDeviceHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(Piix4UsbUhciStub::new().into())
    }
}
