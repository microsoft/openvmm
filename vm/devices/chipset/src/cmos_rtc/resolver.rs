// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for generic CMOS RTC devices.

use super::Rtc;
use async_trait::async_trait;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::cmos_rtc::GenericCmosRtcDeviceHandle;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Resolver for generic CMOS RTC devices.
pub struct GenericCmosRtcResolver;

declare_static_async_resolver! {
    GenericCmosRtcResolver,
    (ChipsetDeviceHandleKind, GenericCmosRtcDeviceHandle),
}

/// Errors that can occur when resolving a generic CMOS RTC device.
#[derive(Debug, Error)]
pub enum ResolveGenericCmosRtcError {
    /// Failed to resolve the runtime clock source.
    #[error("failed to resolve CMOS RTC time source")]
    ResolveTimeSource(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, GenericCmosRtcDeviceHandle>
    for GenericCmosRtcResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolveGenericCmosRtcError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: GenericCmosRtcDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let time_source = resolver
            .resolve(resource.time_source, ())
            .await
            .map_err(ResolveGenericCmosRtcError::ResolveTimeSource)?;

        Ok(Rtc::new(
            time_source.0,
            input
                .configure
                .new_line(IRQ_LINE_SET, "interrupt", resource.irq),
            input.vmtime,
            resource.century_reg_idx,
            resource.initial_cmos,
            false,
        )
        .into())
    }
}
