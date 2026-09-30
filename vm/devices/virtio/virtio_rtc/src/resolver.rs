// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-rtc devices.

use crate::VirtioRtcDevice;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::rtc::VirtioRtcHandle;
use vm_resource::ResolveResource;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-rtc devices.
pub struct VirtioRtcResolver;

declare_static_resolver! {
    VirtioRtcResolver,
    (VirtioDeviceHandle, VirtioRtcHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioRtcHandle> for VirtioRtcResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        _resource: VirtioRtcHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(VirtioRtcDevice::new(input.driver_source).into())
    }
}
