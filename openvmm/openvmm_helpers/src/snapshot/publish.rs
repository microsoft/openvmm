// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writing VM snapshots to a directory.

use super::SnapshotManifest;
use super::format::MAX_SAVED_STATE_SIZE_BYTES;
use anyhow::Context;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// Checks that `dir` can receive a new snapshot. The destination must not
/// exist yet, so existing data is never overwritten or deleted.
pub fn check_snapshot_destination(dir: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => anyhow::bail!("snapshot destination {} already exists", dir.display()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err)
            .with_context(|| format!("failed to access snapshot destination {}", dir.display())),
    }
}

/// Removes the files and directory created by a partially written snapshot
/// unless disarmed.
struct SnapshotCleanup {
    files: Vec<PathBuf>,
    dir: Option<PathBuf>,
}

impl SnapshotCleanup {
    fn disarm(mut self) {
        self.files.clear();
        self.dir = None;
    }
}

impl Drop for SnapshotCleanup {
    fn drop(&mut self) {
        // Remove in reverse creation order so the manifest, if present, goes
        // first and a partially cleaned directory never looks complete.
        for file in self.files.iter().rev() {
            if let Err(err) = std::fs::remove_file(file) {
                if err.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(
                        path = %file.display(),
                        error = &err as &dyn std::error::Error,
                        "failed to clean up partial snapshot file"
                    );
                }
            }
        }
        if let Some(dir) = &self.dir {
            // Only removes the directory if it is empty.
            let _ = std::fs::remove_dir(dir);
        }
    }
}

/// Creates `path`, which must not exist, writes `data` to it and flushes it to
/// disk. Registers `path` for cleanup once the file has been created.
fn write_synced(cleanup: &mut SnapshotCleanup, path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let mut file = fs_err::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    cleanup.files.push(path.to_owned());
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

/// Write a snapshot to the given directory.
///
/// The directory must not exist; it is created by this call, along with any
/// missing parent directories. The snapshot consists of:
/// - `manifest.bin` — protobuf-encoded [`SnapshotManifest`]
/// - `state.bin` — raw device saved-state bytes
/// - `memory.bin` — hard link to the memory backing file
///
/// `state.bin` and `memory.bin` are written first and `manifest.bin` last,
/// each flushed to disk, so the presence of `manifest.bin` indicates that the
/// snapshot was written completely. On failure, the directory and the files
/// created by this call are removed.
pub fn write_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file_path: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        u64::try_from(saved_state_bytes.len()).unwrap_or(u64::MAX) <= MAX_SAVED_STATE_SIZE_BYTES,
        "saved state exceeds the maximum size of {MAX_SAVED_STATE_SIZE_BYTES} bytes"
    );
    check_snapshot_destination(dir)?;
    if let Some(parent) = dir.parent() {
        fs_err::create_dir_all(parent)?;
    }
    // Creating the directory claims the destination, so a concurrent save to
    // the same destination fails here.
    fs_err::create_dir(dir)?;
    let mut cleanup = SnapshotCleanup {
        files: Vec::new(),
        dir: Some(dir.to_owned()),
    };

    // Write device state.
    write_synced(&mut cleanup, &dir.join("state.bin"), saved_state_bytes)
        .context("failed to write state.bin")?;

    // Hard-link memory.bin to the backing file.
    let memory_bin_path = dir.join("memory.bin");
    let canonical_source = fs_err::canonicalize(memory_file_path)?;
    if let Err(err) = std::fs::hard_link(&canonical_source, &memory_bin_path) {
        if err.kind() == io::ErrorKind::CrossesDevices {
            anyhow::bail!(
                "memory backing file ({}) must be on the same filesystem as the snapshot \
                 directory ({})",
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
    cleanup.files.push(memory_bin_path);

    // Write the manifest last.
    let mut manifest = manifest.clone();
    manifest.state_size_bytes = saved_state_bytes.len() as u64;
    let manifest_bytes = mesh::payload::encode(manifest);
    write_synced(&mut cleanup, &dir.join("manifest.bin"), &manifest_bytes)
        .context("failed to write manifest.bin")?;

    cleanup.disarm();
    Ok(())
}
