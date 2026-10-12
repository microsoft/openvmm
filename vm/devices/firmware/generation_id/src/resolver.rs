// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the standalone generation ID device.

use crate::GenerationIdRuntimeDeps;
use crate::device::GenerationIdDevice;
use chipset_device_resources::GPE0_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::generation_id::GenerationIdDeviceHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// The ACPI GPE0 bit used to notify the guest of a generation ID change. This
/// must match the `\_GPE._Exx` method in the DSDT.
pub const GPE0_LINE_GENERATION_ID: u32 = 0;

/// A resolver for standalone generation ID devices.
pub struct GenerationIdResolver;

declare_static_resolver! {
    GenerationIdResolver,
    (ChipsetDeviceHandleKind, GenerationIdDeviceHandle),
}

impl ResolveResource<ChipsetDeviceHandleKind, GenerationIdDeviceHandle> for GenerationIdResolver {
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        resource: GenerationIdDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let GenerationIdDeviceHandle {
            gpa,
            initial_generation_id,
            new_generation_on_restore,
            generation_id_recv,
        } = resource;

        let notify_interrupt =
            input
                .configure
                .new_line(GPE0_LINE_SET, "genid", GPE0_LINE_GENERATION_ID);

        Ok(GenerationIdDevice::new(
            gpa,
            initial_generation_id,
            new_generation_on_restore,
            GenerationIdRuntimeDeps {
                gm: input.encrypted_guest_memory.clone(),
                generation_id_recv,
                notify_interrupt,
            },
        )
        .into())
    }
}
