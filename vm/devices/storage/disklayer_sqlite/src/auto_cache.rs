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
                Ok(disk_id.map(|b| format!("{b:02x}")).join(""))
            },
            anyhow::Ok,
        )?;
        if key.is_empty() {
            anyhow::bail!("empty cache key");
        }
        let path = self.path.join(&key).join("cache.dbhd");

        // Try to open an existing cache file.
        if path.fs_err_try_exists()? {
            let layer = SqliteDiskLayer::new(&path, self.read_only, None)
                .context("failed to open existing cache file")?;
            if layer.sector_count() != metadata.sector_count {
                anyhow::bail!(
                    "cache layer has different sector count: {} vs {}",
                    layer.sector_count(),
                    metadata.sector_count
                );
            }
            return Ok(layer);
        }

        if self.read_only {
            anyhow::bail!("cache file does not exist and cannot be created in read-only mode");
        }

        // Create the cache file atomically: format into a temp file inside
        // a temp directory, then hard-link the db file into the canonical
        // path. The temp directory cleans up the db and its WAL/SHM sidecar
        // files automatically when dropped.
        fs_err::create_dir_all(path.parent().unwrap())?;
        let temp_dir = tempfile::tempdir_in(path.parent().unwrap())
            .context("failed to create temp directory for cache")?;
        let temp_db = temp_dir.path().join("cache.dbhd");

        let format_params = FormatParams {
            logically_read_only: true,
            len: metadata.sector_count * metadata.sector_size as u64,
            sector_size: metadata.sector_size,
        };

        // Format the new cache file, then close before linking.
        drop(SqliteDiskLayer::new(&temp_db, false, Some(format_params))?);

        // hard_link fails if the target already exists, which is how we
        // detect a concurrent creator.
        match fs_err::hard_link(&temp_db, &path) {
            Ok(()) => {
                // We won the race. temp_dir drops and cleans up.
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process created the cache file first.
            }
            Err(e) => {
                return Err(e).context("failed to link cache file into place");
            }
        }

        let layer = SqliteDiskLayer::new(&path, self.read_only, None)?;
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
