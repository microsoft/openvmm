// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolvers for the ICs.

use crate::kvp::KvpIc;
use crate::shutdown::ShutdownIc;
use hyperv_ic_resources::kvp::KvpIcHandle;
use hyperv_ic_resources::shutdown::ShutdownIcHandle;
use std::convert::Infallible;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;

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
