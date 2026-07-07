// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the PIC (Programmable Interrupt Controller) chipset device.

use super::DualPic;
use chipset_device_resources::BSP_LINT_LINE_SET;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::pic::PicDeviceHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for PIC devices.
pub struct PicResolver;

declare_static_resolver! {
    PicResolver,
    (ChipsetDeviceHandleKind, PicDeviceHandle),
}

impl ResolveResource<ChipsetDeviceHandleKind, PicDeviceHandle> for PicResolver {
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: PicDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        // Map IRQ2 to PIC IRQ0 (used by the PIT), since PIC IRQ2 is used to
        // cascade the secondary PIC's output onto the primary.
        //
        // Don't map IRQ0 at all.
        input.configure.add_line_target(IRQ_LINE_SET, 1..=1, 1);
        input.configure.add_line_target(IRQ_LINE_SET, 2..=2, 0);
        input.configure.add_line_target(IRQ_LINE_SET, 3..=15, 3);

        let ready = input.configure.new_line(BSP_LINT_LINE_SET, "ready", 0);

        Ok(DualPic::new(ready, input.register_pio).into())
    }
}
