// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-fs devices backed by an external vhost-user
//! backend.

use crate::VhostUserFsDevice;
use async_trait::async_trait;
use pal_async::socket::PolledSocket;
use unix_socket::UnixStream;
use vhost_user_frontend::VhostUserFrontend;
use vhost_user_protocol::VhostUserSocket;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio::spec::VirtioDeviceType;
use virtio_resources::vhost_user_fs::VhostUserFsHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-fs devices backed by an external vhost-user backend.
pub struct VhostUserFsResolver;

declare_static_async_resolver! {
    VhostUserFsResolver,
    (VirtioDeviceHandle, VhostUserFsHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VhostUserFsHandle> for VhostUserFsResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VhostUserFsHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let driver = input.driver_source.simple();
        let stream = UnixStream::from(resource.socket);
        let polled = PolledSocket::new(&driver, stream)?;
        let socket = VhostUserSocket::new(polled);
        let frontend = VhostUserFrontend::from_socket(driver, socket, VirtioDeviceType::FS).await?;
        let device = VhostUserFsDevice::new(frontend, &resource.tag, resource.num_request_queues);

        Ok(device.into())
    }
}
