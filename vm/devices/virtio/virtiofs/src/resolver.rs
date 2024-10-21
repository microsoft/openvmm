// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Defines the resource resolver for virtiofs devices.

use crate::virtio::VirtioFsDevice;
use crate::VirtioFs;
use virtio::resolve::VirtioResolveInput;
use virtio::VirtioDevice;
use virtio_resources::fs::VirtioFsBackend;
use virtio_resources::fs::VirtioFsHandle;
use vm_resource::declare_static_resolver;
use vm_resource::kind::VirtioDeviceHandle;
use vm_resource::ResolveResource;

/// Resolver for virtiofs devices.
pub struct VirtioFsResolver;

declare_static_resolver! {
    VirtioFsResolver,
    (VirtioDeviceHandle, VirtioFsHandle),
}

impl ResolveResource<VirtioDeviceHandle, VirtioFsHandle> for VirtioFsResolver {
    type Output = Box<dyn VirtioDevice>;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        resource: VirtioFsHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let device = match &resource.fs {
            VirtioFsBackend::HostFs { root_path } => VirtioFsDevice::new(
                input.driver_source,
                &resource.tag,
                VirtioFs::new(root_path, None)?,
                input.guest_memory.clone(),
                0,
                None,
            ),
            #[cfg(windows)]
            VirtioFsBackend::SectionFs { root_path } => {
                VirtioFsDevice::new(
                    input.driver_source,
                    &resource.tag,
                    crate::SectionFs::new(root_path)?,
                    input.guest_memory.clone(),
                    8 * 1024 * 1024 * 1024, // 8GB of shared memory,
                    None,
                )
            }
            #[cfg(not(windows))]
            VirtioFsBackend::SectionFs { .. } => {
                anyhow::bail!("section fs not supported on this platform")
            }
        };
        Ok(Box::new(device))
    }
}
