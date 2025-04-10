// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolvers for the ICs.

use crate::kvp::KvpIc;
use crate::shutdown::ShutdownIc;
use crate::timesync::TimesyncIc;
use anyhow::Context as _;
use async_trait::async_trait;
use hyperv_ic_resources::kvp::KvpIcHandle;
use hyperv_ic_resources::shutdown::ShutdownIcHandle;
use hyperv_ic_resources::timesync::TimesyncIcHandle;
use std::convert::Infallible;
use vm_resource::AsyncResolveResource;
use vm_resource::IntoResource;
use vm_resource::PlatformResource;
use vm_resource::ResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;
use vmcore::reference_time::ReferenceTimeSourceKind;

/// Resource resolver for the shutdown IC.
pub struct ShutdownIcResolver;

declare_static_resolver! {
    ShutdownIcResolver,
    (VmbusDeviceHandleKind, ShutdownIcHandle),
}

impl ResolveResource<VmbusDeviceHandleKind, ShutdownIcHandle> for ShutdownIcResolver {
    type Output = ResolvedVmbusDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        resource: ShutdownIcHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(
            SimpleDeviceWrapper::new(input.driver_source.simple(), ShutdownIc::new(resource.recv))
                .into(),
        )
    }
}

/// Resource resolver for the KVP IC.
pub struct KvpIcResolver;

declare_static_resolver! {
    KvpIcResolver,
    (VmbusDeviceHandleKind, KvpIcHandle),
}

impl ResolveResource<VmbusDeviceHandleKind, KvpIcHandle> for KvpIcResolver {
    type Output = ResolvedVmbusDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        resource: KvpIcHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(
            SimpleDeviceWrapper::new(input.driver_source.simple(), KvpIc::new(resource.recv))
                .into(),
        )
    }
}

/// Resource resolver for the timesync IC.
pub struct TimesyncIcResolver;

declare_static_async_resolver! {
    TimesyncIcResolver,
    (VmbusDeviceHandleKind, TimesyncIcHandle),
}

#[async_trait]
impl AsyncResolveResource<VmbusDeviceHandleKind, TimesyncIcHandle> for TimesyncIcResolver {
    type Output = ResolvedVmbusDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        TimesyncIcHandle: TimesyncIcHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let ref_time = resolver
            .resolve::<ReferenceTimeSourceKind, _>(PlatformResource.into_resource(), ())
            .await
            .context("failed to resolve reference time")?;

        Ok(SimpleDeviceWrapper::new(
            input.driver_source.simple(),
            TimesyncIc::new(&input.driver_source.simple(), ref_time),
        )
        .into())
    }
}
