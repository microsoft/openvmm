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
use virtio_resources::vhost_user::VhostUserBlkHandle;
use virtio_resources::vhost_user::VhostUserFsHandle;
use virtio_resources::vhost_user::VhostUserGenericHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;
use zerocopy::IntoBytes;

/// Resolver for vhost-user frontend devices.
pub struct VhostUserFrontendResolver;

declare_static_async_resolver! {
    VhostUserFrontendResolver,
    (VirtioDeviceHandle, VhostUserGenericHandle),
    (VirtioDeviceHandle, VhostUserFsHandle),
    (VirtioDeviceHandle, VhostUserBlkHandle),
}

/// Connect a vhost-user socket fd and create the frontend.
async fn connect_frontend(
    input: VirtioResolveInput<'_>,
    socket_fd: OwnedFd,
    config: crate::VhostUserConfig,
) -> anyhow::Result<VhostUserFrontend> {
    let driver = input.driver_source.simple();
    let stream = UnixStream::from(socket_fd);
    let polled =
        PolledSocket::new(&driver, stream).context("failed to register vhost-user socket")?;
    let socket = VhostUserSocket::new(polled);

    VhostUserFrontend::from_socket(driver, socket, config)
        .await
        .context("vhost-user handshake failed")
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VhostUserGenericHandle>
    for VhostUserFrontendResolver
{
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VhostUserGenericHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        anyhow::ensure!(
            !resource.queue_sizes.is_empty(),
            "vhost-user generic device requires non-empty queue_sizes"
        );
        let config = crate::VhostUserConfig {
            device_id: VirtioDeviceType(resource.device_id),
            use_backend_config: true,
            queue_sizes: resource.queue_sizes,
            config_patches: vec![],
        };
        let frontend = connect_frontend(input, resource.socket, config).await?;
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

        let num_request_queues = resource.num_queues.unwrap_or(1);
        let queue_size = resource.queue_size.unwrap_or(1024);

        // Total queues = 1 hiprio queue + N request queues.
        let total_queues = 1 + num_request_queues as usize;
        let queue_sizes = vec![queue_size; total_queues];

        // Build the config space locally — the tag is a host-side
        // decision, not sourced from the backend.
        let mut config = virtio_fs::Config {
            tag: [0; virtio_fs::TAG_LEN],
            num_request_queues: (num_request_queues as u32).into(),
        };
        config.tag[..resource.tag.len()].copy_from_slice(resource.tag.as_bytes());

        let vhost_config = crate::VhostUserConfig {
            device_id: VirtioDeviceType::FS,
            use_backend_config: false,
            queue_sizes,
            config_patches: vec![(0, config.as_bytes().to_vec())],
        };

        let frontend = connect_frontend(input, resource.socket, vhost_config)
            .await
            .context("failed to set up vhost-user-fs device")?;

        Ok(frontend.into())
    }
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VhostUserBlkHandle> for VhostUserFrontendResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        resource: VhostUserBlkHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let num_queues = resource.num_queues.unwrap_or(1);
        let queue_size = resource.queue_size.unwrap_or(128);
        let queue_sizes = vec![queue_size; num_queues as usize];

        // Patch the num_queues field in the backend's config space.
        // Config reads are proxied from the backend with this patch
        // applied; writes pass through unchanged.
        let num_queues_offset =
            core::mem::offset_of!(virtio::spec::blk::VirtioBlkConfig, num_queues) as u16;
        let config_patches = vec![(num_queues_offset, num_queues.to_le_bytes().to_vec())];

        let config = crate::VhostUserConfig {
            device_id: VirtioDeviceType::BLK,
            use_backend_config: true,
            queue_sizes,
            config_patches,
        };

        let frontend = connect_frontend(input, resource.socket, config)
            .await
            .context("failed to set up vhost-user-blk device")?;

        Ok(frontend.into())
    }
}
