// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot publication. Artifacts are written and flushed in a private
//! staging directory next to the destination, which is then renamed into
//! place in one operation; failures roll back and clean up the staging
//! directory.

use super::MANIFEST_VERSION;
use super::SnapshotManifest;
use super::format::MANIFEST_FILE_NAME;
use super::format::MAX_MANIFEST_SIZE_BYTES;
use super::format::MAX_SAVED_STATE_SIZE_BYTES;
use super::format::MEMORY_FILE_NAME;
use super::format::STATE_FILE_NAME;
use super::format::validate_manifest_header;
use super::format::validate_manifest_version;
use super::fs::allocated_file_bytes;
use super::fs::copy_exact;
use super::fs::create_private_directory;
use super::fs::ensure_path_absent;
use super::fs::open_regular_file;
use super::fs::path_exists;
use super::fs::rename_no_replace;
use super::fs::snapshot_parent;
use super::fs::sync_directory;
use super::fs::validate_directory;
use super::fs::write_bytes;
use anyhow::Context;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Error publishing a snapshot.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotWriteError {
    /// Nothing was published at the final destination.
    #[error(transparent)]
    BeforeCommit(#[from] anyhow::Error),
    /// The directory rename committed, but its parent could not be flushed.
    #[error("snapshot committed to {path:?}, but flushing its parent directory failed: {error:#}")]
    Committed {
        /// Final committed snapshot path.
        path: PathBuf,
        /// Parent-directory flush failure.
        #[source]
        error: anyhow::Error,
    },
}

impl SnapshotWriteError {
    /// Returns whether the final snapshot directory has been published.
    pub fn is_committed(&self) -> bool {
        matches!(self, Self::Committed { .. })
    }

    /// Returns whether a quiesced source may safely roll back and resume.
    pub fn is_rollback_safe(&self) -> bool {
        matches!(self, Self::BeforeCommit(_))
    }
}

/// Write a snapshot to the given directory.
///
/// The final directory must not already exist. The snapshot consists of:
/// - `manifest.bin` — protobuf-encoded [`SnapshotManifest`]
/// - `state.bin` — raw device saved-state bytes
/// - `memory.bin` — private copy of the memory backing file
///
/// The files are written and flushed in a unique sibling staging directory.
/// The completed staging directory is then renamed to `dir` in one operation.
pub fn write_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file_path: &Path,
) -> Result<(), SnapshotWriteError> {
    let memory_file = open_regular_file(memory_file_path, "memory backing file")?;
    write_snapshot_from_memory_file(dir, manifest, saved_state_bytes, &memory_file)
}

/// Writes a snapshot from the exact open file handle backing guest RAM.
///
/// Capture callers should prefer this over [`write_snapshot`] so replacing the
/// backing pathname cannot substitute different bytes after VM construction.
pub fn write_snapshot_from_memory_file(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
) -> Result<(), SnapshotWriteError> {
    write_snapshot_with_memory_publication(dir, manifest, saved_state_bytes, memory_file)
}

fn write_snapshot_with_memory_publication(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
) -> Result<(), SnapshotWriteError> {
    let mut staging = stage_snapshot(dir, manifest, saved_state_bytes, memory_file)?;
    let commit = openvmm_defs::profile::ProfileSpan::start();
    if let Err(error) =
        ensure_path_absent(dir, "snapshot destination").and_then(|()| staging.publish(dir))
    {
        return Err(staging.rollback(error));
    }
    commit.complete("capture", "publication_commit", Default::default());

    let parent = snapshot_parent(dir);
    let parent_sync = openvmm_defs::profile::ProfileSpan::start();
    sync_directory(parent).map_err(|error| SnapshotWriteError::Committed {
        path: dir.to_owned(),
        error,
    })?;
    parent_sync.complete("capture", "publication_parent_sync", Default::default());
    Ok(())
}

fn stage_snapshot(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
) -> Result<StagingDirectory, SnapshotWriteError> {
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;
    if manifest.version != MANIFEST_VERSION {
        return Err(anyhow::anyhow!(
            "snapshot manifest version {} is not supported for writing (expected {})",
            manifest.version,
            MANIFEST_VERSION,
        )
        .into());
    }
    if u64::try_from(saved_state_bytes.len()).unwrap_or(u64::MAX) > MAX_SAVED_STATE_SIZE_BYTES {
        return Err(anyhow::anyhow!(
            "saved state exceeds the maximum size of {MAX_SAVED_STATE_SIZE_BYTES} bytes"
        )
        .into());
    }

    let parent = snapshot_parent(dir);
    validate_directory(parent, "snapshot parent directory")?;
    ensure_path_absent(dir, "snapshot destination")?;

    let staging = StagingDirectory::create(parent, dir)?;
    let state_path = staging.path().join(STATE_FILE_NAME);
    let memory_path = staging.path().join(MEMORY_FILE_NAME);
    let manifest_path = staging.path().join(MANIFEST_FILE_NAME);

    let result = (|| -> anyhow::Result<()> {
        let state_write = openvmm_defs::profile::ProfileSpan::start();
        write_bytes(&state_path, saved_state_bytes, "saved state")?;
        state_write.complete(
            "capture",
            "publication_state",
            profile_path_counters(&state_path, saved_state_bytes.len() as u64),
        );
        let memory_publish = openvmm_defs::profile::ProfileSpan::start();
        copy_exact(
            memory_file,
            &memory_path,
            manifest.memory_size_bytes,
            "memory backing file",
            "snapshot memory",
        )?;
        memory_publish.complete(
            "capture",
            "publication_memory",
            profile_path_counters(&memory_path, manifest.memory_size_bytes),
        );
        let mut published_manifest = manifest.clone();
        published_manifest.state_size_bytes = saved_state_bytes.len() as u64;
        // Current local snapshots use strict structure and length checks
        // without RAM-sized in-band hashing.
        published_manifest.state_sha256.clear();
        published_manifest.memory_sha256.clear();

        let manifest_bytes = mesh::payload::encode(published_manifest);
        anyhow::ensure!(
            manifest_bytes.len() as u64 <= MAX_MANIFEST_SIZE_BYTES,
            "snapshot manifest exceeds the maximum size of {MAX_MANIFEST_SIZE_BYTES} bytes",
        );
        let manifest_write = openvmm_defs::profile::ProfileSpan::start();
        write_bytes(&manifest_path, &manifest_bytes, "snapshot manifest")?;
        manifest_write.complete(
            "capture",
            "publication_manifest",
            profile_path_counters(&manifest_path, manifest_bytes.len() as u64),
        );

        let staging_sync = openvmm_defs::profile::ProfileSpan::start();
        sync_directory(staging.path())?;
        staging_sync.complete("capture", "publication_staging_sync", Default::default());
        Ok(())
    })();
    match result {
        Ok(()) => Ok(staging),
        Err(error) => Err(staging.rollback(error)),
    }
}

struct StagingDirectory {
    path: Option<PathBuf>,
}

impl StagingDirectory {
    fn create(parent: &Path, destination: &Path) -> anyhow::Result<Self> {
        let destination_name = destination
            .file_name()
            .context("snapshot destination must name a directory")?
            .to_string_lossy();
        let sequence = STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed);

        for attempt in 0..100_u32 {
            let path = parent.join(format!(
                ".{destination_name}.staging-{}-{sequence}-{attempt}",
                std::process::id()
            ));
            match create_private_directory(&path) {
                Ok(()) => {
                    return Ok(Self { path: Some(path) });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("failed to create staging directory {}", path.display())
                    });
                }
            }
        }

        anyhow::bail!(
            "failed to allocate a unique snapshot staging directory in {}",
            parent.display()
        )
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("staging path is present")
    }

    fn publish(&mut self, destination: &Path) -> anyhow::Result<()> {
        let staging_path = self.path();
        rename_no_replace(staging_path, destination).with_context(|| {
            format!(
                "failed to publish snapshot {} to {}",
                staging_path.display(),
                destination.display()
            )
        })?;
        self.path = None;
        Ok(())
    }

    fn rollback(mut self, error: anyhow::Error) -> SnapshotWriteError {
        if let Err(cleanup_error) = self.remove_staging_directory() {
            tracing::warn!(
                error = cleanup_error.as_ref() as &dyn std::error::Error,
                "failed to remove independent snapshot staging artifacts"
            );
        }
        SnapshotWriteError::BeforeCommit(error)
    }

    fn remove_staging_directory(&mut self) -> anyhow::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        let parent = snapshot_parent(path).to_owned();
        match fs_err::remove_dir_all(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to remove staging directory {}", path.display())
                });
            }
        }
        anyhow::ensure!(
            !path_exists(path)?,
            "snapshot staging directory still exists after removal: {}",
            path.display()
        );
        sync_directory(&parent).context("failed to flush snapshot staging cleanup")?;
        self.path = None;
        Ok(())
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(error) = fs_err::remove_dir_all(path) {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    path = %path.display(),
                    "failed to remove dropped snapshot staging directory"
                );
            }
        }
    }
}

fn profile_path_counters(
    path: &Path,
    logical_bytes: u64,
) -> openvmm_defs::profile::ProfileCounters {
    if !openvmm_defs::profile::enabled() {
        return Default::default();
    }
    let allocated_bytes = std::fs::File::open(path)
        .ok()
        .and_then(|file| allocated_file_bytes(&file, logical_bytes).ok());
    openvmm_defs::profile::ProfileCounters {
        logical_bytes: Some(logical_bytes),
        allocated_bytes,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_manifest;
    use super::*;

    #[test]
    fn write_snapshot_rejects_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("a").join("b").join("c");

        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(!err.is_committed());
        assert!(err.to_string().contains("snapshot parent directory"));
        assert!(!snap_dir.exists());
    }

    #[test]
    fn write_snapshot_copies_memory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"SAMEFILE").unwrap();

        let mut manifest = test_manifest();
        manifest.memory_size_bytes = 8;
        write_snapshot(&snap_dir, &manifest, b"state", &mem_path).unwrap();
        std::fs::write(&mem_path, b"MODIFIED").unwrap();

        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            b"SAMEFILE"
        );
    }

    #[test]
    fn exact_memory_handle_survives_path_replacement_before_publish() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        let moved_path = dir.path().join("mapped-memory.bin");
        std::fs::write(&mem_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&mem_path)
            .unwrap();
        std::fs::rename(&mem_path, &moved_path).unwrap();
        std::fs::write(&mem_path, vec![0xa5_u8; 1024]).unwrap();

        write_snapshot_from_memory_file(&snap_dir, &test_manifest(), b"state", &memory_file)
            .unwrap();

        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            vec![0x5a_u8; 1024]
        );
    }

    #[test]
    fn write_snapshot_rejects_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"keep").unwrap();
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(snap_dir.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn publish_does_not_replace_destination_created_after_staging() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mut staging = StagingDirectory::create(dir.path(), &snap_dir).unwrap();
        std::fs::create_dir(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("sentinel"), b"keep").unwrap();

        assert!(staging.publish(&snap_dir).is_err());
        assert_eq!(std::fs::read(snap_dir.join("sentinel")).unwrap(), b"keep");
    }

    #[test]
    fn write_snapshot_rejects_oversized_memory_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1025]).unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("doesn't match manifest"));
        assert!(!snap_dir.exists());
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("staging")
        }));
    }

    #[test]
    fn write_snapshot_rejects_oversized_manifest_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        let mut manifest = test_manifest();
        manifest.openvmm_version = "x".repeat(MAX_MANIFEST_SIZE_BYTES as usize);

        let err = write_snapshot(&snap_dir, &manifest, b"state", &mem_path).unwrap_err();
        assert!(err.to_string().contains("manifest exceeds"));
        assert!(!snap_dir.exists());
    }
}
