// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for the PIIX4 USB UHCI stub device.

use super::Piix4UsbUhciStub;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::piix4_uhci::Piix4PciUsbUhciStubDeviceHandle;
use std::convert::Infallible;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for the PIIX4 USB UHCI stub device.
pub struct Piix4PciUsbUhciStubResolver;

declare_static_resolver! {
    Piix4PciUsbUhciStubResolver,
    (ChipsetDeviceHandleKind, Piix4PciUsbUhciStubDeviceHandle),
}

impl ResolveResource<ChipsetDeviceHandleKind, Piix4PciUsbUhciStubDeviceHandle>
    for Piix4PciUsbUhciStubResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: Piix4PciUsbUhciStubDeviceHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(Piix4UsbUhciStub::new().into())
    }
}
