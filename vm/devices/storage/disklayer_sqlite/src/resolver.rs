// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for sqlite-backed disk layers.

use super::SqliteDiskLayer;
use crate::auto_cache::AutoCacheSqliteDiskLayer;
use crate::FormatOnAttachSqliteDiskLayer;
use disk_backend_resources::layer::SqliteAutoCacheDiskLayerHandle;
use disk_backend_resources::layer::SqliteDiskLayerFormatParams;
use disk_backend_resources::layer::SqliteDiskLayerHandle;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use vm_resource::declare_static_resolver;
use vm_resource::kind::DiskLayerHandleKind;
use vm_resource::ResolveResource;

/// Resolver for a [`SqliteDiskLayerHandle`].
pub struct SqliteDiskLayerResolver;

declare_static_resolver!(
    SqliteDiskLayerResolver,
    (DiskLayerHandleKind, SqliteDiskLayerHandle),
    (DiskLayerHandleKind, SqliteAutoCacheDiskLayerHandle)
);

impl ResolveResource<DiskLayerHandleKind, SqliteDiskLayerHandle> for SqliteDiskLayerResolver {
    type Output = ResolvedDiskLayer;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        rsrc: SqliteDiskLayerHandle,
        input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        let SqliteDiskLayerHandle {
            dbhd_path,
            format_dbhd,
        } = rsrc;

        let layer = if let Some(SqliteDiskLayerFormatParams {
            logically_read_only,
            len,
        }) = format_dbhd
        {
            ResolvedDiskLayer::new(FormatOnAttachSqliteDiskLayer::new(
                dbhd_path.into(),
                input.read_only,
                crate::IncompleteFormatParams {
                    logically_read_only,
                    len,
                },
            ))
        } else {
            ResolvedDiskLayer::new(SqliteDiskLayer::new(
                dbhd_path.as_ref(),
                input.read_only,
                None,
            )?)
        };

        Ok(layer)
    }
}

impl ResolveResource<DiskLayerHandleKind, SqliteAutoCacheDiskLayerHandle>
    for SqliteDiskLayerResolver
{
    type Output = ResolvedDiskLayer;
    type Error = anyhow::Error;

    fn resolve(
        &self,
        rsrc: SqliteAutoCacheDiskLayerHandle,
        input: ResolveDiskLayerParameters<'_>,
    ) -> Result<Self::Output, Self::Error> {
        Ok(ResolvedDiskLayer::new(AutoCacheSqliteDiskLayer::new(
            rsrc.cache_path.into(),
            rsrc.cache_key,
            input.read_only,
        )))
    }
}
