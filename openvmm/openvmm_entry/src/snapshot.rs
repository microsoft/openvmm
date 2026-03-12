// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot manifest types and I/O functions for saving/restoring VM snapshots.

use anyhow::Context;
use mesh::payload::Protobuf;
use std::path::Path;

/// Manifest describing a VM snapshot.
#[derive(Clone, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotManifest {
    /// Manifest format version.
    #[mesh(1)]
    pub version: u32,
    /// Unix timestamp (seconds since epoch) when the snapshot was created.
    #[mesh(2)]
    pub created_at: String,
    /// OpenVMM version that created the snapshot.
    #[mesh(3)]
    pub openvmm_version: String,
    /// Guest RAM size in bytes.
    #[mesh(4)]
    pub memory_size_bytes: u64,
    /// Number of virtual processors.
    #[mesh(5)]
    pub vp_count: u32,
    /// Page size in bytes.
    #[mesh(6)]
    pub page_size: u32,
    /// Architecture string ("x86_64" or "aarch64").
    #[mesh(7)]
    pub architecture: String,
}

/// Write a snapshot to the given directory.
///
/// The directory is created if it does not exist. The snapshot consists of:
/// - `manifest.bin` — protobuf-encoded [`SnapshotManifest`]
/// - `state.bin` — raw device saved-state bytes
/// - `memory.bin` — hard link to the memory backing file
pub fn write_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file_path: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create snapshot directory {}", dir.display()))?;

    // Write manifest.
    let manifest_bytes = mesh::payload::encode(manifest.clone());
    fs_err::write(dir.join("manifest.bin"), &manifest_bytes)
        .context("failed to write manifest.bin")?;

    // Write device state.
    fs_err::write(dir.join("state.bin"), saved_state_bytes).context("failed to write state.bin")?;

    // Handle memory.bin: hard-link from the backing file.
    let memory_bin_path = dir.join("memory.bin");
    let canonical_source = std::fs::canonicalize(memory_file_path)
        .with_context(|| format!("failed to canonicalize {}", memory_file_path.display()))?;

    // If the target already exists (e.g., because the user pointed
    // --memory-backing-file at <dir>/memory.bin directly), check whether
    // source and target are the same file.
    if memory_bin_path.exists() {
        let canonical_target = std::fs::canonicalize(&memory_bin_path)
            .with_context(|| format!("failed to canonicalize {}", memory_bin_path.display()))?;
        if canonical_source == canonical_target {
            // Already the same file — nothing to do.
            return Ok(());
        }
        // Different file at the target path — remove it so the hard link
        // can be created.
        std::fs::remove_file(&memory_bin_path)
            .with_context(|| format!("failed to remove existing {}", memory_bin_path.display()))?;
    }

    if let Err(err) = std::fs::hard_link(&canonical_source, &memory_bin_path) {
        if err.kind() == std::io::ErrorKind::CrossesDevices || err.raw_os_error() == Some(18)
        // EXDEV
        {
            anyhow::bail!(
                "memory backing file ({}) must be on the same filesystem as the snapshot \
                 directory ({}), or pass `--memory-backing-file {}/memory.bin` directly",
                memory_file_path.display(),
                dir.display(),
                dir.display(),
            );
        }
        return Err(err).with_context(|| {
            format!(
                "failed to hard-link {} -> {}",
                canonical_source.display(),
                memory_bin_path.display()
            )
        });
    }

    Ok(())
}

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and the raw saved-state bytes.
/// The caller is responsible for opening `memory.bin` separately.
pub fn read_snapshot(dir: &Path) -> anyhow::Result<(SnapshotManifest, Vec<u8>)> {
    let manifest_bytes =
        fs_err::read(dir.join("manifest.bin")).context("failed to read manifest.bin")?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;

    let state_bytes = fs_err::read(dir.join("state.bin")).context("failed to read state.bin")?;

    Ok((manifest, state_bytes))
}
