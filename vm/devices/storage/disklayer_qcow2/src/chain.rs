// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Qcow2 chain helpers.
//!
//! Functions for opening one or more qcow2 files as a
//! [`LayeredDiskHandle`] ready for resource resolution.

use anyhow::Context;
use disk_backend_resources::DiskLayerDescription;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::Qcow2DiskLayerHandle;
use std::path::Path;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

/// Open a single qcow2 file as a [`LayeredDiskHandle`] with one layer.
///
/// # Errors
///
/// Returns an error if the file cannot be opened.
pub async fn open_qcow2_chain(
    path: &Path,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    open_qcow2_chain_explicit(&[path], read_only).await
}

/// Open a qcow2 chain from an explicit list of file paths.
///
/// `paths` must be ordered from **leaf** (child, index 0) to **base**
/// (parent, last index).
///
/// # Errors
///
/// Returns an error if `paths` is empty or any file cannot be opened.
pub async fn open_qcow2_chain_explicit(
    paths: &[&Path],
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    anyhow::ensure!(!paths.is_empty(), "qcow2 chain must have at least one file");

    let mut layers = Vec::new();
    for (i, path) in paths.iter().enumerate() {
        let is_leaf = i == 0;
        let layer_read_only = !is_leaf || read_only;

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(!layer_read_only)
            .open(path)
            .with_context(|| format!("failed to open qcow2 layer {}: {}", i, path.display()))?;
        let handle = Qcow2DiskLayerHandle {
            file,
            read_only: layer_read_only,
        };
        layers.push(DiskLayerDescription::from(handle.into_resource()));
    }

    Ok(Resource::new(LayeredDiskHandle { layers }))
}
