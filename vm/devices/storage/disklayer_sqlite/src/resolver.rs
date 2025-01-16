// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for sqlite-backed disk layers.

use super::SqliteDiskLayer;
use crate::FormatOnAttachSqliteDiskLayer;
use crate::FormatParams;
use anyhow::Context as _;
use disk_backend_resources::layer::SqliteAutoCacheDiskLayerHandle;
use disk_backend_resources::layer::SqliteDiskLayerFormatParams;
use disk_backend_resources::layer::SqliteDiskLayerHandle;
use disk_layered::resolve::ResolveDiskLayerParameters;
use disk_layered::resolve::ResolvedDiskLayer;
use disk_layered::LayerAttach;
use disk_layered::LayerIo;
use fs_err::PathExt;
use std::path::PathBuf;
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
        let layer = ResolvedDiskLayer::new(AutoCacheSqliteDiskLayer {
            path: rsrc.cache_path.into(),
            key: rsrc.cache_key,
            read_only: input.read_only,
        });
        Ok(layer)
    }
}

struct AutoCacheSqliteDiskLayer {
    path: PathBuf,
    key: Option<String>,
    read_only: bool,
}

impl LayerAttach for AutoCacheSqliteDiskLayer {
    type Error = anyhow::Error;
    type Layer = SqliteDiskLayer;

    async fn attach(
        self,
        lower_layer_metadata: Option<disk_layered::DiskLayerMetadata>,
    ) -> Result<Self::Layer, Self::Error> {
        let metadata = lower_layer_metadata.context("no layer to cache")?;
        let key = self.key.map_or_else(
            || {
                let disk_id = metadata
                    .disk_id
                    .context("cannot cache without a disk ID to use as a key")?;
                Ok(disk_id.map(|b| format!("{b:2x}")).join(""))
            },
            anyhow::Ok,
        )?;
        if key.is_empty() {
            anyhow::bail!("empty cache key");
        }
        let path = self.path.join(key).join("cache.dbhd");
        let format_dbhd = if path.fs_err_try_exists()? || self.read_only {
            None
        } else {
            fs_err::create_dir_all(path.parent().unwrap())?;
            Some(FormatParams {
                logically_read_only: true,
                len: metadata.sector_count * metadata.sector_size as u64,
                sector_size: metadata.sector_size,
            })
        };
        let layer = SqliteDiskLayer::new(&path, self.read_only, format_dbhd)?;
        if layer.sector_count() != metadata.sector_count {
            anyhow::bail!(
                "cache layer has different sector count: {} vs {}",
                layer.sector_count(),
                metadata.sector_count
            );
        }
        Ok(layer)
    }
}
