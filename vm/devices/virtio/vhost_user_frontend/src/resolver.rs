// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for vhost-user frontend devices.

use crate::VhostUserFrontend;
use async_trait::async_trait;
use pal_async::socket::PolledSocket;
use unix_socket::UnixStream;
use vhost_user_protocol::VhostUserSocket;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::vhost_user::VhostUserDeviceHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for vhost-user frontend devices.
pub struct VhostUserFrontendResolver;

declare_static_async_resolver! {
    VhostUserFrontendResolver,
    (VirtioDeviceHandle, VhostUserDeviceHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VhostUserDeviceHandle> for VhostUserFrontendResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VhostUserDeviceHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let driver = input.driver_source.simple();

        // Convert OwnedFd back to UnixStream.
        let stream = UnixStream::from(resource.socket);
        let polled = PolledSocket::new(&driver, stream)?;
        let socket = VhostUserSocket::new(polled);

        let frontend = VhostUserFrontend::from_socket(
            driver,
            socket,
            virtio::spec::VirtioDeviceType(resource.device_id),
        )
        .await?;

        Ok(frontend.into())
    }
}
