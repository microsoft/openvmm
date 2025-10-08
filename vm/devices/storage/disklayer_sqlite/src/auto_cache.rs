// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! [`LayerAttach`] implementation for automatically opening a dbhd to use as a
//! read cache, based on the identity of the next layer.

use crate::FormatParams;
use crate::SqliteDiskLayer;
use anyhow::Context;
use disk_layered::LayerAttach;
use disk_layered::LayerIo;
use fs_err::PathExt;
use std::path::PathBuf;

pub struct AutoCacheSqliteDiskLayer {
    path: PathBuf,
    key: Option<String>,
    read_only: bool,
}

impl AutoCacheSqliteDiskLayer {
    pub fn new(path: PathBuf, key: Option<String>, read_only: bool) -> Self {
        Self {
            path,
            key,
            read_only,
        }
    }
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
