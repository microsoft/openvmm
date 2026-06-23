// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the PIT (Programmable Interval Timer) chipset device.

use super::PitDevice;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pit::PitDeviceHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for PIT devices.
pub struct PitResolver;

declare_static_resolver! {
    PitResolver,
    (ChipsetDeviceHandleKind, PitDeviceHandle),
}

impl ResolveResource<ChipsetDeviceHandleKind, PitDeviceHandle> for PitResolver {
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: PitDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let interrupt = input.configure.new_line(IRQ_LINE_SET, "timer0", 2); // hard-coded IRQ lines, as per x86 spec
        let vmtime = input.vmtime.access("pit");
        Ok(PitDevice::new(interrupt, vmtime).into())
    }
}
