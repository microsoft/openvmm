// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for SQLite-backed disk layers.

use crate::SqliteDisk;
use async_trait::async_trait;
use disk_backend::resolve::ResolveDiskParameters;
use disk_backend::resolve::ResolvedDisk;
use disk_backend_resources::SqliteDiffDiskHandle;
use disk_backend_resources::SqliteDiskHandle;
use std::path::Path;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::DiskHandleKind;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;

/// Resolver for a [`SqliteDiskHandle`] and [`SqliteDiffDiskHandle`].
pub struct SqliteDiskResolver;

declare_static_async_resolver!(
    SqliteDiskResolver,
    (DiskHandleKind, SqliteDiskHandle),
    (DiskHandleKind, SqliteDiffDiskHandle)
);

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, SqliteDiskHandle> for SqliteDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        _resolver: &ResourceResolver,
        rsrc: SqliteDiskHandle,
        input: ResolveDiskParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedDisk::new(SqliteDisk::new(
            rsrc.len,
            Path::new(&rsrc.dbhd_path),
            input.read_only,
        )?)?)
    }
}

#[async_trait]
impl AsyncResolveResource<DiskHandleKind, SqliteDiffDiskHandle> for SqliteDiskResolver {
    type Output = ResolvedDisk;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        rsrc: SqliteDiffDiskHandle,
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
        Ok(ResolvedDisk::new(SqliteDisk::diff(
            lower.0,
            Path::new(&rsrc.dbhd_path),
            input.read_only,
        )?)?)
    }
}
