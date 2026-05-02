// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the ISA DMA chipset device.

use super::DmaController;
use super::ResolvedIsaDmaController;
use chipset_resources::isa_dma::GenericIsaDmaDeviceHandle;
use std::convert::Infallible;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::IsaDmaControllerHandleKind;

/// A resolver for the ISA DMA chipset device.
pub struct GenericIsaDmaResolver;

declare_static_resolver! {
    GenericIsaDmaResolver,
    (IsaDmaControllerHandleKind, GenericIsaDmaDeviceHandle),
}

impl ResolveResource<IsaDmaControllerHandleKind, GenericIsaDmaDeviceHandle>
    for GenericIsaDmaResolver
{
    type Output = ResolvedIsaDmaController;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: GenericIsaDmaDeviceHandle,
        _input: (),
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedIsaDmaController(DmaController::new()))
    }
}
