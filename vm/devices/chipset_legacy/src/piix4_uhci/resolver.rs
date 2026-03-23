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
        resource: Piix4PciUsbUhciStubDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // As per PIIX4 spec, UHCI sits at fixed BDF 00:07.2.
        input
            .configure
            .register_static_pci(resource.pci_bus_name.as_str(), (0, 7, 2));

        Ok(Piix4UsbUhciStub::new().into())
    }
}
