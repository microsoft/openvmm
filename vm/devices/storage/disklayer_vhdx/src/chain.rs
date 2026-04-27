// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX chain helpers.
//!
//! Functions for opening one or more VHDX files as a
//! [`LayeredDiskHandle`](disk_backend_resources::LayeredDiskHandle) ready for
//! resource resolution.

use anyhow::Context;
use disk_backend_resources::DiskLayerDescription;
use disk_backend_resources::LayeredDiskHandle;
use disk_backend_resources::layer::VhdxDiskLayerHandle;
use std::path::Path;
use vm_resource::IntoResource;
use vm_resource::Resource;
use vm_resource::kind::DiskHandleKind;

/// Open a single VHDX file as a [`LayeredDiskHandle`] with one layer.
///
/// Use this for base (non-differencing) VHDX files. For differencing chains,
/// use [`open_vhdx_chain_explicit`] or [`open_vhdx_chain`].
///
/// The file is opened for read+write unless `read_only` is true.
pub fn open_vhdx_single(path: &Path, read_only: bool) -> anyhow::Result<Resource<DiskHandleKind>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .open(path)?;

    Ok(Resource::new(LayeredDiskHandle::single_layer(
        VhdxDiskLayerHandle { file, read_only },
    )))
}

/// Open a VHDX differencing chain from an explicit list of file paths.
///
/// `paths` must be ordered from **leaf** (child, index 0) to **base**
/// (parent, last index). The leaf is opened for read+write (unless
/// `read_only` is true); all parent files are opened read-only.
///
/// Returns a [`LayeredDiskHandle`] with layers ordered top (leaf) to
/// bottom (base), matching the order expected by
/// [`LayeredDisk`](disk_layered::LayeredDisk).
///
/// # Errors
///
/// Returns an error if:
/// - `paths` is empty
/// - Any file cannot be opened
///
/// # Example
///
/// ```no_run
/// # use disklayer_vhdx::chain::open_vhdx_chain_explicit;
/// # use std::path::Path;
/// let resource = open_vhdx_chain_explicit(
///     &[Path::new("child.vhdx"), Path::new("base.vhdx")],
///     false,
/// ).unwrap();
/// ```
pub fn open_vhdx_chain_explicit(
    paths: &[&Path],
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    anyhow::ensure!(!paths.is_empty(), "vhdx chain must have at least one file");

    let layers: Vec<DiskLayerDescription> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let is_leaf = i == 0;
            let layer_read_only = !is_leaf || read_only;

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(!layer_read_only)
                .open(path)
                .with_context(|| format!("failed to open vhdx layer {}: {}", i, path.display()))?;

            let handle = VhdxDiskLayerHandle {
                file,
                read_only: layer_read_only,
            };

            Ok(DiskLayerDescription {
                layer: handle.into_resource(),
                read_cache: false,
                write_through: false,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(Resource::new(LayeredDiskHandle { layers }))
}

/// Open a VHDX differencing chain by auto-walking parent locators.
///
/// Starting from the file at `path`, reads each VHDX file's parent locator
/// to discover the next parent in the chain, continuing until a base
/// (non-differencing) disk is found.
///
/// The leaf file is opened for read+write (unless `read_only` is true);
/// all parent files are opened read-only.
///
/// Parent path resolution order:
/// 1. `relative_path` — resolved relative to the child's directory
/// 2. `absolute_win32_path` — absolute path (platform-dependent)
/// 3. `volume_path` — volume GUID path (Windows-specific)
///
/// # Errors
///
/// Returns an error if:
/// - The leaf file cannot be opened or parsed
/// - A parent locator specifies no usable path
/// - A parent file cannot be found at any of the locator paths
/// - The chain exceeds a reasonable depth limit (detect cycles)
pub async fn open_vhdx_chain(
    path: &Path,
    read_only: bool,
) -> anyhow::Result<Resource<DiskHandleKind>> {
    // Reasonable depth limit to detect cycles or absurdly long chains.
    const MAX_CHAIN_DEPTH: usize = 256;

    let mut paths: Vec<std::path::PathBuf> = vec![path.to_path_buf()];
    let mut current_path = path.to_path_buf();

    loop {
        if paths.len() > MAX_CHAIN_DEPTH {
            anyhow::bail!(
                "vhdx chain exceeds maximum depth of {} — possible cycle",
                MAX_CHAIN_DEPTH
            );
        }

        // Open the current file read-only just to read metadata.
        // The actual read-write open happens later via open_vhdx_chain_explicit.
        let bf = crate::io::BlockingFile::open(&current_path, true)
            .with_context(|| format!("failed to open vhdx file: {}", current_path.display()))?;
        let vhdx = vhdx::VhdxFile::open(bf)
            .read_only()
            .await
            .with_context(|| format!("failed to parse vhdx file: {}", current_path.display()))?;

        if !vhdx.has_parent() {
            // Base disk — chain is complete.
            break;
        }

        // Read the parent locator.
        let locator = vhdx
            .parent_locator()
            .await
            .with_context(|| {
                format!(
                    "failed to read parent locator from: {}",
                    current_path.display()
                )
            })?
            .context("differencing disk has no parent locator")?;

        let parent_paths = locator.parent_paths();
        let child_dir = current_path.parent().unwrap_or_else(|| Path::new("."));

        // Try to resolve the parent path in order of preference.
        let parent_path = resolve_parent_path(child_dir, &parent_paths).with_context(|| {
            format!(
                "could not find parent for vhdx file: {}",
                current_path.display()
            )
        })?;

        paths.push(parent_path.clone());
        current_path = parent_path;
    }

    // Convert PathBufs to Path references for open_vhdx_chain_explicit.
    let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
    open_vhdx_chain_explicit(&path_refs, read_only)
}

/// Try to resolve a parent path from the locator's well-known keys.
///
/// Tries paths in order: relative_path, absolute_win32_path, volume_path.
/// Returns the first path that exists on disk, or an error if none work.
fn resolve_parent_path(
    child_dir: &Path,
    parent_paths: &vhdx::ParentPaths,
) -> anyhow::Result<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();

    // 1. Relative path — resolve relative to the child's directory.
    if let Some(ref rel) = parent_paths.relative_path {
        // VHDX relative paths use Windows separators (backslash).
        // Normalize to the platform's separator.
        let normalized: String = rel
            .chars()
            .map(|c| {
                if c == '\\' {
                    std::path::MAIN_SEPARATOR
                } else {
                    c
                }
            })
            .collect();
        // Strip leading ".\" or "./" if present.
        let stripped = normalized
            .strip_prefix(&format!(".{}", std::path::MAIN_SEPARATOR))
            .unwrap_or(&normalized);
        candidates.push(child_dir.join(stripped));
    }

    // 2. Absolute Win32 path (Windows-specific).
    if cfg!(windows) {
        if let Some(ref abs) = parent_paths.absolute_win32_path {
            candidates.push(std::path::PathBuf::from(abs));
        }
    }

    // 3. Volume path (Windows-specific).
    if cfg!(windows) {
        if let Some(ref vol) = parent_paths.volume_path {
            candidates.push(std::path::PathBuf::from(vol));
        }
    }

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    if candidates.is_empty() {
        anyhow::bail!("parent locator contains no path entries");
    }

    // None of the candidates exist. Report all attempted paths.
    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    anyhow::bail!("parent not found at any locator path: {}", tried.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_single_creates_one_layer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vhdx");

        let path2 = path.clone();
        pal_async::DefaultPool::run_with(|_driver| async move {
            let bf = crate::io::BlockingFile::open(&path2, false).unwrap();
            let mut params = vhdx::CreateParams {
                disk_size: 1024 * 1024,
                ..Default::default()
            };
            vhdx::create(&bf, &mut params).await.unwrap();
        });

        let resource = open_vhdx_single(&path, false).unwrap();
        let _ = resource;
    }

    #[test]
    fn explicit_chain_empty_errors() {
        let result = open_vhdx_chain_explicit(&[], false);
        assert!(result.is_err());
    }

    #[test]
    fn explicit_chain_single_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.vhdx");

        let path2 = path.clone();
        pal_async::DefaultPool::run_with(|_driver| async move {
            let bf = crate::io::BlockingFile::open(&path2, false).unwrap();
            let mut params = vhdx::CreateParams {
                disk_size: 1024 * 1024,
                ..Default::default()
            };
            vhdx::create(&bf, &mut params).await.unwrap();
        });

        let resource = open_vhdx_chain_explicit(&[path.as_path()], false).unwrap();
        let _ = resource;
    }

    #[test]
    fn explicit_chain_missing_file_errors() {
        let result = open_vhdx_chain_explicit(&[Path::new("nonexistent.vhdx")], false);
        assert!(result.is_err());
    }

    #[pal_async::async_test]
    async fn auto_walk_base_disk() {
        // Create a base (non-differencing) VHDX, then auto-walk it.
        // Should produce a single-layer chain.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("base.vhdx");

        let bf = crate::io::BlockingFile::open(&path, false).unwrap();
        let mut params = vhdx::CreateParams {
            disk_size: 1024 * 1024,
            ..Default::default()
        };
        vhdx::create(&bf, &mut params).await.unwrap();
        drop(bf);

        let resource = open_vhdx_chain(&path, false).await.unwrap();
        let _ = resource;
    }
}
