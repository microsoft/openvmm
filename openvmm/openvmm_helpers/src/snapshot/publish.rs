// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Writing VM snapshots to a directory.

use super::SnapshotManifest;
use anyhow::Context;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// Returns whether `a` and `b` refer to the same underlying file (as opposed
/// to merely having the same path). Hard links to the same file are reported
/// as the same file.
fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(a)?;
        let b = std::fs::metadata(b)?;
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(windows)]
    {
        let canonical_a = fs_err::canonicalize(a)?;
        let canonical_b = fs_err::canonicalize(b)?;
        if canonical_a == canonical_b {
            return Ok(true);
        }
        // File IDs are only unique within a volume. Files on different volumes
        // (different path prefixes) can't be hard links of each other.
        if canonical_a.components().next() != canonical_b.components().next() {
            return Ok(false);
        }
        let id = |path: &Path| -> io::Result<i64> {
            Ok(pal::windows::fs::query_stat_lx(&std::fs::File::open(path)?)?.FileId)
        };
        match (id(a), id(b)) {
            (Ok(id_a), Ok(id_b)) => Ok(id_a == id_b),
            // The file system can't report file IDs; treat the files as
            // different so the caller fails safe.
            _ => Ok(false),
        }
    }
}

/// Checks that `dir` can receive a new snapshot of a VM whose memory is backed
/// by `memory_file_path`, without overwriting or deleting existing data.
///
/// Fails if `dir` exists but is not a directory, if it already contains a
/// snapshot (`manifest.bin`), or if it contains a `memory.bin` that is not the
/// same file as `memory_file_path`. A `memory.bin` that is the backing file
/// itself (by path or by hard link) is allowed.
pub fn check_snapshot_destination(dir: &Path, memory_file_path: &Path) -> anyhow::Result<()> {
    match std::fs::metadata(dir) {
        Ok(metadata) => {
            if !metadata.is_dir() {
                anyhow::bail!("snapshot destination {} is not a directory", dir.display());
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to access snapshot destination {}", dir.display())
            });
        }
    }

    let manifest_path = dir.join("manifest.bin");
    if manifest_path
        .try_exists()
        .with_context(|| format!("failed to access {}", manifest_path.display()))?
    {
        anyhow::bail!(
            "snapshot destination {} already contains a snapshot",
            dir.display()
        );
    }

    let memory_bin_path = dir.join("memory.bin");
    if memory_bin_path
        .try_exists()
        .with_context(|| format!("failed to access {}", memory_bin_path.display()))?
    {
        let same = same_file(&memory_bin_path, memory_file_path).with_context(|| {
            format!(
                "failed to compare {} with {}",
                memory_bin_path.display(),
                memory_file_path.display()
            )
        })?;
        if !same {
            anyhow::bail!(
                "{} already exists and is not the VM's memory backing file",
                memory_bin_path.display()
            );
        }
    }

    Ok(())
}

/// Removes the files and directory created by a partially written snapshot
/// unless disarmed.
#[derive(Default)]
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

/// Writes `data` to `path` and flushes it to disk. Registers `path` for
/// cleanup once the file has been opened by this function.
fn write_synced(
    cleanup: &mut SnapshotCleanup,
    path: &Path,
    data: &[u8],
    create_new: bool,
) -> anyhow::Result<()> {
    let mut options = fs_err::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
    let mut file = options.open(path)?;
    cleanup.files.push(path.to_owned());
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

/// Write a snapshot to the given directory.
///
/// The directory is created if it does not exist. The snapshot consists of:
/// - `manifest.bin` — protobuf-encoded [`SnapshotManifest`]
/// - `state.bin` — raw device saved-state bytes
/// - `memory.bin` — hard link to the memory backing file
///
/// The destination is first checked with [`check_snapshot_destination`], so
/// an existing snapshot or unrelated `memory.bin` is never overwritten or
/// deleted. `state.bin` and `memory.bin` are written first and `manifest.bin`
/// last, each flushed to disk, so the presence of `manifest.bin` indicates
/// that the snapshot was written completely. On failure, the files (and
/// directory) created by this call are removed.
pub fn write_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file_path: &Path,
) -> anyhow::Result<()> {
    check_snapshot_destination(dir, memory_file_path)?;

    let mut cleanup = SnapshotCleanup::default();
    if !dir.try_exists()? {
        cleanup.dir = Some(dir.to_owned());
    }
    fs_err::create_dir_all(dir)?;

    // Write device state.
    write_synced(
        &mut cleanup,
        &dir.join("state.bin"),
        saved_state_bytes,
        false,
    )
    .context("failed to write state.bin")?;

    // Hard-link memory.bin to the backing file, unless the destination check
    // found that it already is the backing file.
    let memory_bin_path = dir.join("memory.bin");
    if !memory_bin_path.try_exists()? {
        let canonical_source = fs_err::canonicalize(memory_file_path)?;
        if let Err(err) = std::fs::hard_link(&canonical_source, &memory_bin_path) {
            if err.kind() == io::ErrorKind::CrossesDevices {
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
        cleanup.files.push(memory_bin_path);
    }

    // Write the manifest last.
    let manifest_bytes = mesh::payload::encode(manifest.clone());
    write_synced(
        &mut cleanup,
        &dir.join("manifest.bin"),
        &manifest_bytes,
        true,
    )
    .context("failed to write manifest.bin")?;

    cleanup.disarm();
    Ok(())
}
