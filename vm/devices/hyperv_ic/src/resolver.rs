// Copyright (C) Microsoft Corporation. All rights reserved.

//! Resource resolvers for the ICs.

use crate::shutdown::ShutdownIc;
use hyperv_ic_resources::shutdown::ShutdownIcHandle;
use std::convert::Infallible;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::ResolveResource;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;

/// Resource resolver for the ICs.
pub struct IcResolver;

declare_static_resolver! {
    IcResolver,
    (VmbusDeviceHandleKind, ShutdownIcHandle),
}

impl ResolveResource<VmbusDeviceHandleKind, ShutdownIcHandle> for IcResolver {
    type Output = ResolvedVmbusDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        resource: ShutdownIcHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(SimpleDeviceWrapper::new(
            input.driver_source.simple(),
            ShutdownIc::new(input.driver_source.simple(), resource.recv),
        )
        .into())
    }
}
