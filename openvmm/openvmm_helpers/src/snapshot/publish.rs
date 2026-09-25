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
use super::format::SCRATCH_FILE_NAME;
use super::format::STATE_FILE_NAME;
use super::format::validate_manifest_header;
use super::format::validate_manifest_version;
use super::fs::OpenedSnapshotDirectory;
use super::fs::allocated_file_bytes;
use super::fs::copy_exact;
use super::fs::create_hard_link_from_handle;
use super::fs::create_private_directory;
use super::fs::ensure_path_absent;
use super::fs::hard_link_is_unsupported;
use super::fs::open_file_with_length;
use super::fs::open_regular_file;
use super::fs::path_exists;
use super::fs::rename_no_replace;
use super::fs::snapshot_parent;
use super::fs::sync_directory;
use super::fs::validate_directory;
use super::fs::verify_file_digest;
use super::fs::verify_hard_link_identity;
use super::fs::write_bytes;
use super::microvm;
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
    /// Publication did not commit, but an alias to live source RAM may remain.
    ///
    /// Callers must terminate rather than resume the source. The reported
    /// private staging path may require operator cleanup after termination.
    #[error(
        "snapshot failed before commit at {path:?}, and automatic RAM staging alias cleanup is uncertain: publication error: {error:#}; cleanup error: {cleanup_error:#}"
    )]
    CleanupUncertain {
        /// Staging directory that may retain an alias to live source RAM.
        path: PathBuf,
        /// Original publication failure.
        error: anyhow::Error,
        /// Failure removing or proving absence of the staging alias.
        #[source]
        cleanup_error: anyhow::Error,
    },
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
    write_snapshot_from_memory_and_scratch_files(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        None,
    )
}

/// Writes a snapshot with an optional scratch image paired to the VM state.
pub fn write_snapshot_from_memory_and_scratch_files(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
) -> Result<(), SnapshotWriteError> {
    write_snapshot_with_memory_publication(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        scratch_file,
        MemoryPublication::IndependentCopy,
    )
}

/// Writes a snapshot by promoting OpenVMM-owned guest RAM as an exact file.
///
/// The caller must use this only for automatic backing whose lifetime it owns,
/// and must stop all writers before calling. It must terminate the source after
/// success or [`SnapshotWriteError::Committed`]. A
/// [`SnapshotWriteError::BeforeCommit`] proves that staging was removed and the
/// source may resume. [`SnapshotWriteError::CleanupUncertain`] requires source
/// termination because a private staging alias may remain. Unsupported hard
/// links fall back to an independent sparse-aware copy.
pub fn write_snapshot_from_owned_memory_and_scratch_files(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
) -> Result<(), SnapshotWriteError> {
    write_snapshot_with_memory_publication(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        scratch_file,
        MemoryPublication::OwnedExactFile,
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryPublication {
    IndependentCopy,
    OwnedExactFile,
}

fn write_snapshot_with_memory_publication(
    dir: &Path,
    manifest: &SnapshotManifest,
    saved_state_bytes: &[u8],
    memory_file: &std::fs::File,
    scratch_file: Option<&std::fs::File>,
    memory_publication: MemoryPublication,
) -> Result<(), SnapshotWriteError> {
    let mut staging = stage_snapshot(
        dir,
        manifest,
        saved_state_bytes,
        memory_file,
        scratch_file,
        memory_publication,
    )?;
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
    scratch_file: Option<&std::fs::File>,
    memory_publication: MemoryPublication,
) -> Result<StagingDirectory, SnapshotWriteError> {
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        microvm::validate_machine_contract_shape(
            contract,
            manifest.memory_size_bytes,
            manifest.vp_count,
        )?;
    }
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

    let mut staging = StagingDirectory::create(parent, dir)?;
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
        if memory_publication == MemoryPublication::IndependentCopy {
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
        }
        match (microvm::paired_scratch_block(manifest), scratch_file) {
            (Some(scratch), Some(scratch_file)) => {
                let scratch_path = staging.path().join(SCRATCH_FILE_NAME);
                let scratch_publish = openvmm_defs::profile::ProfileSpan::start();
                copy_exact(
                    scratch_file,
                    &scratch_path,
                    scratch.length,
                    "scratch backing file",
                    "snapshot scratch",
                )?;
                verify_file_digest(
                    &open_file_with_length(&scratch_path, scratch.length, SCRATCH_FILE_NAME)?,
                    scratch.length,
                    &scratch.identity,
                    "scratch.img",
                )?;
                scratch_publish.complete(
                    "capture",
                    "publication_scratch",
                    profile_path_counters(&scratch_path, scratch.length),
                );
            }
            (Some(_), None) => anyhow::bail!("snapshot contract requires a paired scratch image"),
            (None, Some(_)) => {
                anyhow::bail!("snapshot contract does not declare a scratch image")
            }
            (None, None) => {}
        }

        let mut published_manifest = manifest.clone();
        published_manifest.state_size_bytes = saved_state_bytes.len() as u64;
        // Current local snapshots use strict structure, length, generation,
        // and machine-contract checks without RAM-sized in-band hashing.
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

        if memory_publication == MemoryPublication::OwnedExactFile {
            let memory_publish = openvmm_defs::profile::ProfileSpan::start();
            publish_owned_memory_file(&mut staging, memory_file, manifest.memory_size_bytes)?;
            memory_publish.complete(
                "capture",
                "publication_memory",
                profile_path_counters(&memory_path, manifest.memory_size_bytes),
            );
        }

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
    source_memory_alias: bool,
    #[cfg(test)]
    inject_cleanup_failure: bool,
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
                    return Ok(Self {
                        path: Some(path),
                        source_memory_alias: false,
                        #[cfg(test)]
                        inject_cleanup_failure: false,
                    });
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

    fn mark_source_memory_alias(&mut self) {
        self.source_memory_alias = true;
    }

    fn rollback(mut self, error: anyhow::Error) -> SnapshotWriteError {
        if self.source_memory_alias
            && let Err(cleanup_error) = self.remove_staging_directory()
        {
            return SnapshotWriteError::CleanupUncertain {
                path: self
                    .path
                    .clone()
                    .expect("unpublished staging path is present"),
                error,
                cleanup_error,
            };
        }

        if let Err(cleanup_error) = self.remove_staging_directory() {
            tracing::warn!(
                error = cleanup_error.as_ref() as &dyn std::error::Error,
                "failed to remove independent snapshot staging artifacts"
            );
        }
        SnapshotWriteError::BeforeCommit(error)
    }

    fn remove_staging_directory(&mut self) -> anyhow::Result<()> {
        #[cfg(test)]
        if self.inject_cleanup_failure {
            anyhow::bail!("injected snapshot staging cleanup failure");
        }
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
        self.source_memory_alias = false;
        Ok(())
    }
}

impl Drop for StagingDirectory {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(error) = fs_err::remove_dir_all(path) {
                tracing::error!(
                    error = &error as &dyn std::error::Error,
                    source_memory_alias = self.source_memory_alias,
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

fn publish_owned_memory_file(
    staging: &mut StagingDirectory,
    source: &std::fs::File,
    expected_length: u64,
) -> anyhow::Result<()> {
    let source_metadata = source
        .metadata()
        .context("failed to inspect automatic snapshot RAM handle")?;
    anyhow::ensure!(
        source_metadata.file_type().is_file(),
        "automatic snapshot RAM handle is not a regular file"
    );
    anyhow::ensure!(
        source_metadata.len() == expected_length,
        "automatic snapshot RAM size ({} bytes) doesn't match manifest ({expected_length} bytes)",
        source_metadata.len()
    );

    // The VM worker has stopped all writers before this call. Flush through
    // the exact handle immediately before linking the same file generation.
    source
        .sync_all()
        .context("failed to flush automatic snapshot RAM handle")?;

    let memory_path = staging.path().join(MEMORY_FILE_NAME);
    let directory = OpenedSnapshotDirectory::open_for_publication(staging.path())?;
    // From this point, any failure is treated as though the link may have been
    // installed until the complete staging directory is proven absent.
    staging.mark_source_memory_alias();
    let method = match create_hard_link_from_handle(source, &directory.file, MEMORY_FILE_NAME) {
        Ok(method) => method,
        Err(error) if hard_link_is_unsupported(&error) => {
            tracing::info!(
                error = &error as &dyn std::error::Error,
                "exact-file snapshot RAM publication is unavailable; using independent copy"
            );
            drop(directory);
            return copy_exact(
                source,
                &memory_path,
                expected_length,
                "automatic snapshot RAM handle",
                "snapshot memory",
            );
        }
        Err(error) => {
            return Err(error).context("failed to create automatic RAM staging hard link");
        }
    };
    let linked =
        directory.open_regular_file_for_identity(MEMORY_FILE_NAME, "linked snapshot memory")?;
    verify_hard_link_identity(source, &linked, expected_length)?;
    tracing::info!(
        method,
        logical_bytes = expected_length,
        "published exact snapshot memory artifact"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::format::SHA256_SIZE;
    use super::super::fs::file_sha256;
    use super::super::fs::initialize_snapshot_memory_backing_file;
    use super::super::microvm::paired_scratch_manifest;
    use super::super::restore::open_paired_scratch_file;
    use super::super::restore::read_snapshot_manifest;
    use super::super::tests::test_manifest;
    use super::*;
    use std::io::Seek;
    use std::io::SeekFrom;
    use std::io::Write;

    #[test]
    fn paired_scratch_is_published_and_verified() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("memory.bin");
        let scratch_path = dir.path().join("scratch.img");
        let scratch = vec![0x5a_u8; 1024];
        std::fs::write(&memory_path, vec![0_u8; 1024]).unwrap();
        std::fs::write(&scratch_path, &scratch).unwrap();
        let memory_file = std::fs::File::open(memory_path).unwrap();
        let scratch_file = std::fs::File::open(scratch_path).unwrap();
        let manifest = paired_scratch_manifest(&scratch);

        write_snapshot_from_memory_and_scratch_files(
            &snap_dir,
            &manifest,
            b"state",
            &memory_file,
            Some(&scratch_file),
        )
        .unwrap();

        let read_manifest = read_snapshot_manifest(&snap_dir).unwrap();
        let verified = open_paired_scratch_file(&snap_dir, &read_manifest)
            .unwrap()
            .unwrap();
        assert_eq!(
            file_sha256(&verified, 1024, SCRATCH_FILE_NAME).unwrap(),
            manifest.machine_contract.unwrap().microvm_sandbox_blocks[1].identity
        );
        drop(verified);

        let published = snap_dir.join(SCRATCH_FILE_NAME);
        std::fs::write(&published, vec![0xa5_u8; 1024]).unwrap();
        assert!(
            open_paired_scratch_file(&snap_dir, &read_manifest)
                .unwrap_err()
                .to_string()
                .contains("digest mismatch")
        );

        std::fs::write(&published, vec![0_u8; 512]).unwrap();
        assert!(
            open_paired_scratch_file(&snap_dir, &read_manifest)
                .unwrap_err()
                .to_string()
                .contains("doesn't match manifest")
        );

        std::fs::remove_file(&published).unwrap();
        let error = match read_snapshot_manifest(&snap_dir) {
            Ok(_) => panic!("snapshot without scratch.img unexpectedly validated"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("incomplete"));
    }

    #[test]
    fn paired_scratch_digest_mismatch_is_not_published() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("memory.bin");
        let scratch_path = dir.path().join("scratch.img");
        let scratch = vec![0x5a_u8; 1024];
        std::fs::write(&memory_path, vec![0_u8; 1024]).unwrap();
        std::fs::write(&scratch_path, &scratch).unwrap();
        let memory_file = std::fs::File::open(memory_path).unwrap();
        let scratch_file = std::fs::File::open(scratch_path).unwrap();
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .microvm_sandbox_blocks[1]
            .identity = vec![0xff; SHA256_SIZE];

        let error = write_snapshot_from_memory_and_scratch_files(
            &snap_dir,
            &manifest,
            b"state",
            &memory_file,
            Some(&scratch_file),
        )
        .unwrap_err();
        assert!(error.to_string().contains("digest mismatch"));
        assert!(!snap_dir.exists());
    }

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

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn owned_memory_snapshot_publishes_exact_file() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("automatic-memory.bin");
        std::fs::write(&memory_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&memory_path)
            .unwrap();

        write_snapshot_from_owned_memory_and_scratch_files(
            &snap_dir,
            &test_manifest(),
            b"state",
            &memory_file,
            None,
        )
        .unwrap();

        let published = std::fs::File::open(snap_dir.join(MEMORY_FILE_NAME)).unwrap();
        verify_hard_link_identity(&memory_file, &published, 1024).unwrap();
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn owned_memory_link_uses_exact_handle_after_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("automatic-memory.bin");
        let moved_path = dir.path().join("mapped-memory.bin");
        std::fs::write(&memory_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&memory_path)
            .unwrap();
        std::fs::rename(&memory_path, &moved_path).unwrap();
        std::fs::write(&memory_path, vec![0xa5_u8; 1024]).unwrap();

        write_snapshot_from_owned_memory_and_scratch_files(
            &snap_dir,
            &test_manifest(),
            b"state",
            &memory_file,
            None,
        )
        .unwrap();

        let published = std::fs::File::open(snap_dir.join(MEMORY_FILE_NAME)).unwrap();
        verify_hard_link_identity(&memory_file, &published, 1024).unwrap();
        assert_eq!(
            std::fs::read(snap_dir.join(MEMORY_FILE_NAME)).unwrap(),
            vec![0x5a_u8; 1024]
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn injected_failure_removes_live_memory_staging_alias() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_path = dir.path().join("automatic-memory.bin");
        std::fs::write(&memory_path, vec![0x5a_u8; 1024]).unwrap();
        let memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&memory_path)
            .unwrap();
        let staging = stage_snapshot(
            &snap_dir,
            &test_manifest(),
            b"state",
            &memory_file,
            None,
            MemoryPublication::OwnedExactFile,
        )
        .unwrap();
        assert!(staging.source_memory_alias);
        let staging_path = staging.path().to_owned();

        let error = staging.rollback(anyhow::anyhow!("injected failure after memory link"));

        assert!(error.is_rollback_safe());
        assert!(!staging_path.exists());
        assert!(!snap_dir.exists());
        std::fs::write(&memory_path, vec![0xa5_u8; 1024]).unwrap();
    }

    #[test]
    fn uncertain_live_alias_cleanup_is_not_rollback_safe() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mut staging = StagingDirectory::create(dir.path(), &snap_dir).unwrap();
        staging.mark_source_memory_alias();
        staging.inject_cleanup_failure = true;

        let error = staging.rollback(anyhow::anyhow!("injected publication failure"));

        assert!(!error.is_committed());
        assert!(!error.is_rollback_safe());
        assert!(matches!(error, SnapshotWriteError::CleanupUncertain { .. }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_snapshot_preserves_sparse_memory_and_clone_independence() {
        const MEMORY_SIZE: u64 = 16 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        for (case, sparse) in [("zero", true), ("mixed", true), ("dense", false)] {
            let case_dir = dir.path().join(case);
            std::fs::create_dir(&case_dir).unwrap();
            let snap_dir = case_dir.join("snap");
            let mem_path = case_dir.join("memory.bin");
            let mut memory_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&mem_path)
                .unwrap();
            assert_eq!(
                initialize_snapshot_memory_backing_file(&memory_file, MEMORY_SIZE).unwrap(),
                0
            );

            let mut expected = vec![0_u8; MEMORY_SIZE as usize];
            match case {
                "zero" => {}
                "mixed" => {
                    let tail_offset = expected.len() - 4;
                    expected[..4].copy_from_slice(b"head");
                    expected[tail_offset..].copy_from_slice(b"tail");
                    memory_file.write_all(b"head").unwrap();
                    memory_file.seek(SeekFrom::End(-4)).unwrap();
                    memory_file.write_all(b"tail").unwrap();
                }
                "dense" => {
                    expected.fill(0x5a);
                    memory_file.seek(SeekFrom::Start(0)).unwrap();
                    memory_file.write_all(&expected).unwrap();
                }
                _ => unreachable!(),
            }
            memory_file.sync_all().unwrap();

            let mut manifest = test_manifest();
            manifest.memory_size_bytes = MEMORY_SIZE;
            write_snapshot_from_memory_file(&snap_dir, &manifest, b"state", &memory_file).unwrap();

            memory_file.seek(SeekFrom::Start(0)).unwrap();
            memory_file.write_all(b"xxxx").unwrap();
            memory_file.sync_all().unwrap();

            let published_path = snap_dir.join(MEMORY_FILE_NAME);
            let published = std::fs::File::open(&published_path).unwrap();
            assert_eq!(published.metadata().unwrap().len(), MEMORY_SIZE);
            if sparse {
                let allocated = allocated_file_bytes(&published, MEMORY_SIZE).unwrap();
                assert!(
                    allocated < MEMORY_SIZE / 4,
                    "{case} snapshot allocated {allocated} bytes for a {MEMORY_SIZE}-byte file",
                );
            }
            assert_eq!(std::fs::read(published_path).unwrap(), expected, "{case}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn write_snapshot_uses_dense_memory_files_on_windows() {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
        const MEMORY_SIZE: u64 = 4 * 1024 * 1024;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        let mut memory_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(mem_path)
            .unwrap();
        initialize_snapshot_memory_backing_file(&memory_file, MEMORY_SIZE).unwrap();
        assert_eq!(
            memory_file.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE,
            0
        );

        memory_file.write_all(b"head").unwrap();
        memory_file.seek(SeekFrom::End(-4)).unwrap();
        memory_file.write_all(b"tail").unwrap();
        memory_file.sync_all().unwrap();

        let mut manifest = test_manifest();
        manifest.memory_size_bytes = MEMORY_SIZE;
        write_snapshot_from_memory_file(&snap_dir, &manifest, b"state", &memory_file).unwrap();

        let published_path = snap_dir.join(MEMORY_FILE_NAME);
        let published = std::fs::File::open(&published_path).unwrap();
        assert_eq!(
            published.metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE,
            0
        );

        let mut expected = vec![0_u8; MEMORY_SIZE as usize];
        expected[..4].copy_from_slice(b"head");
        let tail_offset = expected.len() - 4;
        expected[tail_offset..].copy_from_slice(b"tail");
        assert_eq!(std::fs::read(published_path).unwrap(), expected);
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
