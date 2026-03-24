// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the generic ISA DMA controller.

use super::DmaController;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::isa_dma::GenericIsaDmaDeviceHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Resolver for the generic ISA DMA controller device.
pub struct GenericIsaDmaResolver;

declare_static_resolver! {
    GenericIsaDmaResolver,
    (ChipsetDeviceHandleKind, GenericIsaDmaDeviceHandle),
}

impl ResolveResource<ChipsetDeviceHandleKind, GenericIsaDmaDeviceHandle> for GenericIsaDmaResolver {
    type Output = ResolvedChipsetDevice;
    type Error = std::convert::Infallible;

    fn resolve(
        &self,
        _resource: GenericIsaDmaDeviceHandle,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(DmaController::new().into())
    }
}
