// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for RAM disks.

use super::Error;
use super::RamDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedSimpleDisk;
use disk_backend_resources::RamDiffDiskHandle;
use disk_backend_resources::RamDiskHandle;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;

/// Resolver for a [`RamDiskHandle`] and [`RamDiffDiskHandle`].
pub struct RamDiskResolver;

declare_static_async_resolver!(
    RamDiskResolver,
    (DiskHandleKind, RamDiskHandle),
    (DiskHandleKind, RamDiffDiskHandle)
);

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, RamDiskHandle> for RamDiskResolver {
    type Output = ResolvedSimpleDisk;
    type Error = Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: RamDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(RamDisk::new(rsrc.len, input.read_only)?.into())
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, RamDiffDiskHandle> for RamDiskResolver {
    type Output = ResolvedSimpleDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        rsrc: RamDiffDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let lower = resolver
            .resolve(
                rsrc.lower,
                ResolveDiskParameters {
                    read_only: true,
                    _async_trait_workaround: &(),
                },
            )
            .await?;
        Ok(RamDisk::diff(lower.0, input.read_only)?.into())
    }
}
