// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::DelayDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::DelayDiskHandle;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;

// A resolver for DelayDisk.
pub struct DelayDiskResolver;
declare_static_async_resolver!(DelayDiskResolver, (DiskHandleKind, DelayDiskHandle));

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, DelayDiskHandle> for DelayDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        rsrc: DelayDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let inner = resolver.resolve(rsrc.disk, input).await?;

        ResolvedDisk::new(DelayDisk::new(rsrc.delay, inner.0, inner.0.ta))
            .map_err(|e| anyhow::anyhow!("failed to create delay disk: {}", e))
    }
}
