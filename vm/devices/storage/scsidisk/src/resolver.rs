// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver for SCSI devices.

use crate::SimpleScsiDisk;
use crate::scsidvd::SimpleScsiDvd;
use anyhow::Context;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use futures::StreamExt;
use pal_async::task::Spawn;
use scsi_core::ResolveScsiDeviceHandleParams;
use scsi_core::ResolvedScsiDevice;
use scsidisk_resources::SimpleScsiDiskHandle;
use scsidisk_resources::SimpleScsiDvdHandle;
use scsidisk_resources::SimpleScsiDvdRequest;
use std::sync::Arc;
use std::sync::Weak;
use thiserror::Error;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::ScsiDeviceHandleKind;

/// A resolver for [`SimpleScsiDiskHandle`] and [`SimpleScsiDvdHandle`].
pub struct SimpleScsiResolver;

declare_static_async_resolver! {
    SimpleScsiResolver,
    (ScsiDeviceHandleKind, SimpleScsiDiskHandle),
    (ScsiDeviceHandleKind, SimpleScsiDvdHandle),
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to resolve backing disk")]
    Disk(#[source] ResolveError),
}

#[async_trait]
impl AsyncResolveResource<ScsiDeviceHandleKind, SimpleScsiDiskHandle> for SimpleScsiResolver {
    type Output = ResolvedScsiDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SimpleScsiDiskHandle,
        _: ResolveScsiDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = resolver
            .resolve(
                resource.disk,
                ResolveDiskParameters {
                    read_only: resource.read_only,
                    _async_trait_workaround: &(),
                },
            )
            .await
            .map_err(Error::Disk)?;

        let disk = SimpleScsiDisk::new(disk.0, resource.parameters);
        Ok(disk.into())
    }
}

#[async_trait]
impl AsyncResolveResource<ScsiDeviceHandleKind, SimpleScsiDvdHandle> for SimpleScsiResolver {
    type Output = ResolvedScsiDevice;
    type Error = Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: SimpleScsiDvdHandle,
        input: ResolveScsiDeviceHandleParams<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let media = if let Some(media) = resource.media {
            Some(
                resolver
                    .resolve(
                        media,
                        ResolveDiskParameters {
                            read_only: true,
                            _async_trait_workaround: &(),
                        },
                    )
                    .await
                    .map_err(Error::Disk)?
                    .0,
            )
        } else {
            None
        };
        let dvd = Arc::new(SimpleScsiDvd::new(media));

        // Start a task to handle incoming change media requests.
        if let Some(requests) = resource.requests {
            input
                .driver_source
                .simple()
                .spawn(
                    "dvd-requests",
                    handle_dvd_requests(Arc::downgrade(&dvd), resolver.clone(), requests),
                )
                .detach();
        }

        Ok(ResolvedScsiDevice(dvd))
    }
}

async fn handle_dvd_requests(
    dvd: Weak<SimpleScsiDvd>,
    resolver: ResourceResolver,
    mut requests: mesh::Receiver<SimpleScsiDvdRequest>,
) {
    while let Some(req) = requests.next().await {
        match req {
            SimpleScsiDvdRequest::ChangeMedia(rpc) => {
                rpc.handle_failable(async |resource| {
                    let media = if let Some(resource) = resource {
                        Some(
                            resolver
                                .resolve(
                                    resource,
                                    ResolveDiskParameters {
                                        read_only: true,
                                        _async_trait_workaround: &(),
                                    },
                                )
                                .await
                                .context("failed to resolve media")?
                                .0,
                        )
                    } else {
                        None
                    };
                    if let Some(dvd) = dvd.upgrade() {
                        dvd.change_media(media);
                    }
                    anyhow::Ok(())
                })
                .await
            }
        }
    }
}
