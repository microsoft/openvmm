// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver implementation for the guest crash device.

use crate::GuestCrashDevice;
use get_resources::crash::GuestCrashDeviceHandle;
use std::convert::Infallible;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vm_resource::ResolveResource;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;

/// Resource resolver for [`GuestCrashDeviceHandle`].
pub struct GuestCrashDeviceResolver;

declare_static_resolver!(
    GuestCrashDeviceResolver,
    (VmbusDeviceHandleKind, GuestCrashDeviceHandle)
);

impl ResolveResource<VmbusDeviceHandleKind, GuestCrashDeviceHandle> for GuestCrashDeviceResolver {
    type Output = ResolvedVmbusDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        resource: GuestCrashDeviceHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(SimpleDeviceWrapper::new(
            input.driver_source.simple(),
            GuestCrashDevice::new(resource.request_dump, resource.max_dump_size),
        )
        .into())
    }
}
