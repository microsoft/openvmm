// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::GuestEmulationLog;
use get_resources::gel::GuestEmulationLogHandle;
use std::convert::Infallible;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VmbusDeviceHandleKind;
use vmbus_channel::resources::ResolveVmbusDeviceHandleParams;
use vmbus_channel::resources::ResolvedVmbusDevice;
use vmbus_channel::simple::SimpleDeviceWrapper;

/// Resource resolver for [`GuestEmulationLogHandle`].
pub struct GuestEmulationLogResolver;

declare_static_resolver!(
    GuestEmulationLogResolver,
    (VmbusDeviceHandleKind, GuestEmulationLogHandle)
);

impl ResolveResource<VmbusDeviceHandleKind, GuestEmulationLogHandle> for GuestEmulationLogResolver {
    type Output = ResolvedVmbusDevice;
    type Error = Infallible;

    fn resolve(
        &self,
        _resource: GuestEmulationLogHandle,
        input: ResolveVmbusDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(SimpleDeviceWrapper::new(input.driver_source.simple(), GuestEmulationLog::new()).into())
    }
}
