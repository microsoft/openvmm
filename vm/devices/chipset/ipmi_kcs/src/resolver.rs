// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for the IPMI KCS chipset device.

use crate::IpmiKcs;
use crate::TrustedClock;
use crate::device::IpmiKcsDevice;
use async_trait::async_trait;
use chipset_device_resources::ResolveChipsetDeviceHandleParams;
use chipset_device_resources::ResolvedChipsetDevice;
use chipset_resources::CmosRtcTimeSourceHandleKind;
use chipset_resources::ResolvedCmosRtcTimeSource;
use chipset_resources::ipmi_kcs::IpmiKcsDeviceHandleAArch64;
use chipset_resources::ipmi_kcs::IpmiKcsDeviceHandleX64;
use chipset_resources::ipmi_kcs::IpmiSelEventSinkHandleKind;
use chipset_resources::ipmi_kcs::ResolvedIpmiSelEventSink;
use local_clock::InspectableLocalClock;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::Resource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ChipsetDeviceHandleKind;

/// Resource resolver for IPMI KCS device handles.
pub struct IpmiKcsResolver;

declare_static_async_resolver! {
    IpmiKcsResolver,
    (ChipsetDeviceHandleKind, IpmiKcsDeviceHandleX64),
    (ChipsetDeviceHandleKind, IpmiKcsDeviceHandleAArch64),
}

/// Error resolving an IPMI KCS device.
#[derive(Debug, Error)]
pub enum ResolveIpmiKcsError {
    /// The trusted time source could not be resolved.
    #[error("failed to resolve IPMI KCS time source")]
    TimeSource(#[source] ResolveError),
    /// The SEL event sink could not be resolved.
    #[error("failed to resolve IPMI SEL event sink")]
    EventSink(#[source] ResolveError),
}

struct TrustedClockAdapter(Box<dyn InspectableLocalClock>);

impl TrustedClock for TrustedClockAdapter {
    fn unix_seconds(&mut self) -> i64 {
        self.0
            .get_time()
            .as_millis_since_unix_epoch()
            .div_euclid(1000)
    }
}

async fn resolve_core(
    resolver: &ResourceResolver,
    event_sink: Resource<IpmiSelEventSinkHandleKind>,
    time_source: Resource<CmosRtcTimeSourceHandleKind>,
) -> Result<IpmiKcs, ResolveIpmiKcsError> {
    let ResolvedCmosRtcTimeSource(time_source) = resolver
        .resolve(time_source, ())
        .await
        .map_err(ResolveIpmiKcsError::TimeSource)?;
    let ResolvedIpmiSelEventSink(event_sink) = resolver
        .resolve(event_sink, ())
        .await
        .map_err(ResolveIpmiKcsError::EventSink)?;

    Ok(IpmiKcs::with_event_sink(
        Box::new(TrustedClockAdapter(time_source)),
        event_sink,
    ))
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, IpmiKcsDeviceHandleX64> for IpmiKcsResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveIpmiKcsError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: IpmiKcsDeviceHandleX64,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let core = resolve_core(resolver, resource.event_sink, resource.time_source).await?;
        Ok(IpmiKcsDevice::new_pio(core).into())
    }
}

#[async_trait]
impl AsyncResolveResource<ChipsetDeviceHandleKind, IpmiKcsDeviceHandleAArch64> for IpmiKcsResolver {
    type Output = ResolvedChipsetDevice;
    type Error = ResolveIpmiKcsError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: IpmiKcsDeviceHandleAArch64,
        _input: ResolveChipsetDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let core = resolve_core(resolver, resource.event_sink, resource.time_source).await?;
        Ok(IpmiKcsDevice::new_mmio(core).into())
    }
}
