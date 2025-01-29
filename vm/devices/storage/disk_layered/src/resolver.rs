// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolvers for layered disks.

use super::resolve::ResolveDiskLayerParameters;
use super::resolve::ResolvedDiskLayer;
use super::InvalidLayeredDisk;
use super::LayerConfiguration;
use super::LayeredDisk;
use crate::DiskLayer;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend::InvalidDisk;
use disk_backend_resources::layer::DiskLayerHandle;
use disk_backend_resources::LayeredDiskHandle;
use futures::future::TryJoinAll;
use thiserror::Error;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResolveError;
use vm_resource::ResourceResolver;

declare_static_async_resolver! {
    LayeredDiskResolver,
    (DiskHandleKind, LayeredDiskHandle),
    (DiskLayerHandleKind, DiskLayerHandle)
}

/// Resolver for [`LayeredDiskHandle`] and [`DiskLayerHandle`].
pub struct LayeredDiskResolver;

/// Error type for [`LayeredDiskResolver`].
#[derive(Debug, Error)]
pub enum ResolveLayeredDiskError {
    /// Failed to resolve a layer resource.
    #[error("failed to resolve layer {0}")]
    ResolveLayer(usize, #[source] ResolveError),
    /// Failed to create the layered disk.
    #[error("failed to create layered disk")]
    CreateDisk(#[source] InvalidLayeredDisk),
    /// Failed to instantiate the disk.
    #[error("invalid disk")]
    InvalidDisk(#[source] InvalidDisk),
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, LayeredDiskHandle> for LayeredDiskResolver {
    type Output = ResolvedDisk;
    type Error = ResolveLayeredDiskError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: LayeredDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let mut read_only = input.read_only;
        let layers = resource
            .layers
            .into_iter()
            .enumerate()
            .map(|(i, desc)| {
                let this_read_only = read_only && !desc.read_cache;
                if !desc.write_through {
                    read_only = true;
                }
                async move {
                    let layer = resolver
                        .resolve(
                            desc.layer,
                            ResolveDiskLayerParameters {
                                read_only: this_read_only,
                                _async_trait_workaround: &(),
                            },
                        )
                        .await
                        .map_err(|err| ResolveLayeredDiskError::ResolveLayer(i, err))?;

                    Ok(LayerConfiguration {
                        layer: layer.0,
                        write_through: desc.write_through,
                        read_cache: desc.read_cache,
                    })
                }
            })
            .collect::<TryJoinAll<_>>()
            .await?;

        let disk = LayeredDisk::new(input.read_only, layers)
            .await
            .map_err(ResolveLayeredDiskError::CreateDisk)?;

        ResolvedDisk::new(disk).map_err(ResolveLayeredDiskError::InvalidDisk)
    }
}

#[async_trait]
impl AsyncResolveResource<DiskLayerHandleKind, DiskLayerHandle> for LayeredDiskResolver {
    type Output = ResolvedDiskLayer;
    type Error = ResolveError;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: DiskLayerHandle,
        input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let disk = resolver
            .resolve(
                resource.0,
                ResolveDiskParameters {
                    read_only: input.read_only,
                    _async_trait_workaround: &(),
                },
            )
            .await?;

        Ok(ResolvedDiskLayer(DiskLayer::from_disk(disk.0)))
    }
}
