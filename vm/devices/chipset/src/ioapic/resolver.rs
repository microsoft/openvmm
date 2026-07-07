// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the generic IO-APIC chipset device.

use super::IoApicDevice;
use async_trait::async_trait;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::ioapic::GenericIoApicDeviceHandle;
use chipset_resources::ioapic::IOAPIC_NUM_ENTRIES;
use chipset_resources::ioapic::IoApicRoutingHandleKind;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// A resolver for generic IO-APIC devices.
pub struct GenericIoApicResolver;

declare_static_async_resolver! {
    GenericIoApicResolver,
    (ChipsetDeviceHandleKind, GenericIoApicDeviceHandle),
}

/// Errors that can occur when resolving a generic IO-APIC device.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum ResolveGenericIoApicError {
    #[error("failed to resolve ioapic routing")]
    ResolveRouting(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, GenericIoApicDeviceHandle>
    for GenericIoApicResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveGenericIoApicError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: GenericIoApicDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let routing = resolver
            .resolve::<IoApicRoutingHandleKind, _>(resource.routing, ())
            .await
            .map_err(ResolveGenericIoApicError::ResolveRouting)?;

        input
            .configure
            .add_line_target(IRQ_LINE_SET, 0..=IOAPIC_NUM_ENTRIES as u32 - 1, 0);

        Ok(IoApicDevice::new(IOAPIC_NUM_ENTRIES, routing.0).into())
    }
}
