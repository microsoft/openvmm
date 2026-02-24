// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the PIT (Programmable Interval Timer) chipset device.

use super::PitDevice;
use async_trait::async_trait;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pit::PitDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for PIT devices.
pub struct PitResolver;

declare_static_async_resolver! {
    PitResolver,
    (ChipsetDeviceHandleKind, PitDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, PitDeviceHandle> for PitResolver {
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        _resource: PitDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Hard-coded to IRQ line 2, as per x86 spec
        let interrupt = input.configure.new_line(IRQ_LINE_SET, "timer0", 2);
        let vmtime = input.vmtime.access("pit");
        Ok(PitDevice::new(interrupt, vmtime).into())
    }
}
