// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for block device disk handles.

use super::BlockDevice;
use super::NewDeviceError;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::BlockDeviceDiskHandle;
use scsi_buffers::BounceBufferTracker;
use std::fmt::Debug;
use std::sync::Arc;
use thiserror::Error;
use uevent::UeventListener;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;

pub struct BlockDeviceResolver {
    uevent_listener: Option<Arc<UeventListener>>,
    bounce_buffer_tracker: Arc<BounceBufferTracker>,
    always_bounce: bool,
}

impl BlockDeviceResolver {
    pub fn new(
        uevent_listener: Option<Arc<UeventListener>>,
        bounce_buffer_tracker: Arc<BounceBufferTracker>,
        always_bounce: bool,
    ) -> Self {
        Self {
            uevent_listener,
            bounce_buffer_tracker,
            always_bounce,
        }
    }
}

#[derive(Debug, Error)]
pub enum ResolveDiskError {
    #[error("failed to create new device")]
    NewDevice(#[source] NewDeviceError),
    #[error("invalid disk")]
    InvalidDisk(#[source] disk_backend::InvalidDisk),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, BlockDeviceDiskHandle> for BlockDeviceResolver {
    type Output = ResolvedDisk;
    type Error = ResolveDiskError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: BlockDeviceDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = BlockDevice::new(
            rsrc.file,
            input.read_only,
            input.driver_source.simple(),
            self.uevent_listener.as_deref(),
            Some(self.bounce_buffer_tracker.clone()),
            self.always_bounce,
        )
        .await
        .map_err(ResolveDiskError::NewDevice)?;
        ResolvedDisk::new(disk).map_err(ResolveDiskError::InvalidDisk)
    }
}

/// A static resolver for [`BlockDeviceDiskHandle`] that does not use a
/// shared [`BounceBufferTracker`]. Bounce buffers are allocated on demand
/// without rate limiting.
pub struct StaticBlockDeviceResolver;

declare_static_async_resolver!(
    StaticBlockDeviceResolver,
    (DiskHandleKind, BlockDeviceDiskHandle),
);

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, BlockDeviceDiskHandle> for StaticBlockDeviceResolver {
    type Output = ResolvedDisk;
    type Error = ResolveDiskError;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: BlockDeviceDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = BlockDevice::new(
            rsrc.file,
            input.read_only,
            input.driver_source.simple(),
            None,
            None,
            false,
        )
        .await
        .map_err(ResolveDiskError::NewDevice)?;
        ResolvedDisk::new(disk).map_err(ResolveDiskError::InvalidDisk)
    }
}
