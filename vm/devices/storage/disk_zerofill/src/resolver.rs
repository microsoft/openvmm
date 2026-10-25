// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The resolver of [`ZeroFillDiskHandle`].

use crate::ZeroFillDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::ZeroFillDiskHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;

/// A resolver of [`ZeroFillDiskHandle`] to a [`ZeroFillDisk`].
pub struct ZeroFillDiskResolver;
declare_static_async_resolver!(ZeroFillDiskResolver, (DiskHandleKind, ZeroFillDiskHandle));

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, ZeroFillDiskHandle> for ZeroFillDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        rsrc: ZeroFillDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let backing = match rsrc.disk {
            Some(disk) => Some(resolver.resolve(disk, input).await?.0),
            None => None,
        };
        let disk = ZeroFillDisk::new(
            backing,
            rsrc.sector_count,
            rsrc.sector_size,
            rsrc.physical_sector_size,
            input.read_only,
        )?;
        Ok(ResolvedDisk::new(disk)?)
    }
}
