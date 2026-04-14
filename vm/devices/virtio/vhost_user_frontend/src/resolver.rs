// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for vhost-user frontend devices.

use crate::VhostUserFrontend;
use anyhow::Context as _;
use async_trait::async_trait;
use pal_async::socket::PolledSocket;
use std::os::fd::OwnedFd;
use unix_socket::UnixStream;
use vhost_user_protocol::VhostUserSocket;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio::spec::VirtioDeviceType;
use virtio::spec::fs as virtio_fs;
use virtio_resources::vhost_user::VhostUserDeviceHandle;
use virtio_resources::vhost_user::VhostUserFsHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;
use zerocopy::IntoBytes;

/// Resolver for vhost-user frontend devices.
pub struct VhostUserFrontendResolver;

declare_static_async_resolver! {
    VhostUserFrontendResolver,
    (VirtioDeviceHandle, VhostUserDeviceHandle),
    (VirtioDeviceHandle, VhostUserFsHandle),
}

/// Connect a vhost-user socket fd and create the frontend.
async fn connect_frontend(
    input: VirtioResolveInput<'_>,
    socket_fd: OwnedFd,
    device_id: VirtioDeviceType,
    config_space: Option<Vec<u8>>,
) -> anyhow::Result<VhostUserFrontend> {
    let driver = input.driver_source.simple();
    let stream = UnixStream::from(socket_fd);
    let polled =
        PolledSocket::new(&driver, stream).context("failed to register vhost-user socket")?;
    let socket = VhostUserSocket::new(polled);

    VhostUserFrontend::from_socket(driver, socket, device_id, config_space)
        .await
        .context("vhost-user handshake failed")
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
        let frontend = connect_frontend(
            input,
            resource.socket,
            VirtioDeviceType(resource.device_id),
            None,
        )
        .await?;
        Ok(frontend.into())
    }
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VhostUserFsHandle> for VhostUserFrontendResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VhostUserFsHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        anyhow::ensure!(
            resource.tag.len() <= virtio_fs::TAG_LEN,
            "virtiofs tag {:?} exceeds maximum length of {} bytes",
            resource.tag,
            virtio_fs::TAG_LEN,
        );

        // Build the config space locally — the tag is a host-side
        // decision, not sourced from the backend.
        let mut config = virtio_fs::Config {
            tag: [0; virtio_fs::TAG_LEN],
            num_request_queues: 1.into(),
        };
        config.tag[..resource.tag.len()].copy_from_slice(resource.tag.as_bytes());

        let frontend = connect_frontend(
            input,
            resource.socket,
            VirtioDeviceType::FS,
            Some(config.as_bytes().to_vec()),
        )
        .await
        .context("failed to set up vhost-user-fs device")?;

        Ok(frontend.into())
    }
}
