// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for PIIX4 CMOS RTC devices.

use super::Piix4CmosRtc;
use async_trait::async_trait;
use chipset_device_resources::IRQ_LINE_SET;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::cmos_rtc::Piix4CmosRtcDeviceHandle;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Resolver for PIIX4 CMOS RTC devices.
pub struct Piix4CmosRtcResolver;

declare_static_async_resolver! {
    Piix4CmosRtcResolver,
    (ChipsetDeviceHandleKind, Piix4CmosRtcDeviceHandle),
}

/// Errors that can occur when resolving a PIIX4 CMOS RTC device.
#[derive(Debug, Error)]
pub enum ResolvePiix4CmosRtcError {
    /// Failed to resolve the runtime clock source.
    #[error("failed to resolve CMOS RTC time source")]
    ResolveTimeSource(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, Piix4CmosRtcDeviceHandle>
    for Piix4CmosRtcResolver
{
    type Output = ResolvedChipsetDevice;
    type Error = ResolvePiix4CmosRtcError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: Piix4CmosRtcDeviceHandle,
        input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let time_source = resolver
            .resolve(resource.time_source, ())
            .await
            .map_err(ResolvePiix4CmosRtcError::ResolveTimeSource)?;

        // Hard-coded to IRQ line 8, as per PIIX4 spec.
        let rtc_interrupt = input.configure.new_line(IRQ_LINE_SET, "interrupt", 8);

        Ok(Piix4CmosRtc::new(
            time_source.0,
            rtc_interrupt,
            input.vmtime,
            resource.initial_cmos,
            resource.enlightened_interrupts,
        )
        .into())
    }
}
