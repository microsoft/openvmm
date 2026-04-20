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
            match SqliteDiskLayer::new(&path, self.read_only, None) {
                Ok(layer) => {
                    if layer.sector_count() != metadata.sector_count {
                        anyhow::bail!(
                            "cache layer has different sector count: {} vs {}",
                            layer.sector_count(),
                            metadata.sector_count
                        );
                    }
                    return Ok(layer);
                }
                Err(e) if self.read_only => {
                    return Err(e).context("failed to open cache file in read-only mode");
                }
                Err(e) => {
                    // The cache file exists but is corrupt (e.g., from a
                    // previous interrupted format). Delete it and re-create.
                    tracing::warn!(
                        path = %path.display(),
                        error = &*e,
                        "corrupt cache file, removing and re-creating"
                    );
                    remove_sqlite_file(&path);
                }
            }
        } else if self.read_only {
            anyhow::bail!("cache file does not exist and cannot be created in read-only mode");
        }

        // Create the cache file atomically: format into a temp file, then
        // hard-link into place. If a concurrent process wins the link, we
        // discard our temp file and open the winner's file instead.
        fs_err::create_dir_all(path.parent().unwrap())?;
        let temp_path = path.with_file_name(format!("cache.dbhd.tmp.{}", std::process::id()));

        let format_params = FormatParams {
            logically_read_only: true,
            len: metadata.sector_count * metadata.sector_size as u64,
            sector_size: metadata.sector_size,
        };

        // Format the new cache file at the temp path, then close before linking.
        drop(SqliteDiskLayer::new(
            &temp_path,
            false,
            Some(format_params),
        )?);

        // hard_link fails if the target already exists, which is how we
        // detect a concurrent creator.
        match fs_err::hard_link(&temp_path, &path) {
            Ok(()) => {
                // We won the race. Remove the temp entry (the hard link is
                // the canonical path now).
                remove_sqlite_file(&temp_path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process created the cache file first.
                remove_sqlite_file(&temp_path);
            }
            Err(e) => {
                remove_sqlite_file(&temp_path);
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

/// Remove a SQLite database file and its WAL/SHM sidecar files.
fn remove_sqlite_file(path: &std::path::Path) {
    let _ = fs_err::remove_file(path);
    let name = path.file_name().unwrap().to_str().unwrap();
    let _ = fs_err::remove_file(path.with_file_name(format!("{name}-wal")));
    let _ = fs_err::remove_file(path.with_file_name(format!("{name}-shm")));
}
