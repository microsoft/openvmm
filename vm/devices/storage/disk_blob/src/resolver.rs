// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resolver implementation for [`BlobDisk`].

use crate::blob::http::HttpBlob;
use crate::BlobDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::BlobDiskFormat;
use disk_backend_resources::BlobDiskHandle;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;

/// A resolver for blob disks.
pub struct BlobDiskResolver;

declare_static_async_resolver!(BlobDiskResolver, (DiskHandleKind, BlobDiskHandle));

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, BlobDiskHandle> for BlobDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: BlobDiskHandle,
        params: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        if !params.read_only {
            anyhow::bail!("writable blob disks not supported");
        }

        let blob = HttpBlob::new(&rsrc.url).await?;
        let disk = match rsrc.format {
            BlobDiskFormat::Flat => BlobDisk::new(blob),
            BlobDiskFormat::FixedVhd1 => BlobDisk::new_fixed_vhd1(blob).await?,
        };

        Ok(ResolvedDisk::new(disk)?)
    }
}
