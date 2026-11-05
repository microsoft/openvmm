// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writing VM snapshots to a directory.

use super::SnapshotManifest;
use anyhow::Context;
use std::path::Path;

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
    fs_err::create_dir_all(dir)?;

    // Write manifest.
    let manifest_bytes = mesh::payload::encode(manifest.clone());
    fs_err::write(dir.join("manifest.bin"), &manifest_bytes)?;

    // Write device state.
    fs_err::write(dir.join("state.bin"), saved_state_bytes)?;

    // Handle memory.bin: hard-link from the backing file.
    let memory_bin_path = dir.join("memory.bin");
    let canonical_source = fs_err::canonicalize(memory_file_path)?;

    // Check whether source and target are already the same file (e.g.,
    // the user pointed --memory-backing-file at <dir>/memory.bin directly).
    let needs_link = if memory_bin_path.exists() {
        let canonical_target = fs_err::canonicalize(&memory_bin_path)?;
        if canonical_source == canonical_target {
            false
        } else {
            // Different file at the target path — remove it so the hard
            // link can be created.
            fs_err::remove_file(&memory_bin_path)?;
            true
        }
    } else {
        true
    };

    if needs_link {
        if let Err(err) = std::fs::hard_link(&canonical_source, &memory_bin_path) {
            if err.kind() == std::io::ErrorKind::CrossesDevices {
                anyhow::bail!(
                    "memory backing file ({}) must be on the same filesystem as the snapshot \
                     directory ({}); consider placing the backing file inside the snapshot \
                     directory",
                    memory_file_path.display(),
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
    }

    Ok(())
}
