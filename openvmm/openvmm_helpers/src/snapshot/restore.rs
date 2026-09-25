// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restore-side snapshot access: opening one published generation through a
//! retained directory handle, and structural validation against its manifest.

use super::SnapshotManifest;
use super::format::MANIFEST_FILE_NAME;
use super::format::MAX_MANIFEST_SIZE_BYTES;
use super::format::MAX_SAVED_STATE_SIZE_BYTES;
use super::format::MEMORY_FILE_NAME;
use super::format::STATE_FILE_NAME;
use super::format::validate_manifest_header;
use super::format::validate_manifest_version;
use super::fs::OpenedFileGeneration;
use super::fs::OpenedSnapshotDirectory;
use super::fs::allocated_file_bytes;
use super::fs::opened_file_generation;
use super::fs::read_bounded_open_file;
use anyhow::Context;
use std::collections::HashSet;
use std::path::Path;

/// One structurally validated snapshot generation opened for restore.
///
/// Artifact access is relative to the retained directory handle. The open
/// artifact handles keep pathname replacement from substituting another
/// generation after validation.
pub struct OpenedSnapshot {
    directory: OpenedSnapshotDirectory,
    manifest_file: std::fs::File,
    state_file: std::fs::File,
    memory_file: std::fs::File,
    memory_generation: OpenedFileGeneration,
    manifest: SnapshotManifest,
    state_bytes: Vec<u8>,
}

impl OpenedSnapshot {
    /// Opens and structurally validates one exact snapshot generation.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let directory = OpenedSnapshotDirectory::open(dir)?;
        let manifest_file = directory.open_regular_file(MANIFEST_FILE_NAME, "snapshot manifest")?;
        let state_file = directory.open_regular_file(STATE_FILE_NAME, "saved state")?;
        let memory_file = directory.open_regular_file(MEMORY_FILE_NAME, "snapshot memory")?;
        let manifest = decode_snapshot_manifest(&manifest_file)?;
        validate_snapshot_directory(&directory)?;
        anyhow::ensure!(
            manifest.state_size_bytes <= MAX_SAVED_STATE_SIZE_BYTES,
            "state.bin length in the manifest exceeds the maximum size of \
             {MAX_SAVED_STATE_SIZE_BYTES} bytes",
        );

        let state_bytes =
            read_bounded_open_file(&state_file, MAX_SAVED_STATE_SIZE_BYTES, "saved state")?;
        anyhow::ensure!(
            state_bytes.len() as u64 == manifest.state_size_bytes,
            "state.bin size ({} bytes) doesn't match manifest ({} bytes)",
            state_bytes.len(),
            manifest.state_size_bytes,
        );

        let memory_generation = opened_file_generation(&memory_file, MEMORY_FILE_NAME)?;
        anyhow::ensure!(
            memory_generation.length() == manifest.memory_size_bytes,
            "memory.bin size ({} bytes) doesn't match manifest ({} bytes)",
            memory_generation.length(),
            manifest.memory_size_bytes,
        );

        Ok(Self {
            directory,
            manifest_file,
            state_file,
            memory_file,
            memory_generation,
            manifest,
            state_bytes,
        })
    }

    /// Returns the authoritative manifest read from this opened generation.
    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }

    /// Returns the saved-state bytes read from this opened generation.
    pub fn state_bytes(&self) -> &[u8] {
        &self.state_bytes
    }

    /// Returns total logical and allocated bytes for the opened artifacts.
    ///
    /// This is intended for opt-in profiling. Restore validation does not
    /// depend on allocation accounting being available.
    pub fn artifact_size_counters(&self) -> anyhow::Result<(u64, u64)> {
        [&self.manifest_file, &self.state_file, &self.memory_file]
            .into_iter()
            .try_fold((0_u64, 0_u64), |(logical, allocated), file| {
                let length = file.metadata()?.len();
                Ok((
                    logical
                        .checked_add(length)
                        .context("logical byte count overflow")?,
                    allocated
                        .checked_add(allocated_file_bytes(file, length)?)
                        .context("allocated byte count overflow")?,
                ))
            })
    }

    /// Duplicates the exact memory handle used to create a private mapping.
    pub fn duplicate_memory_file_for_mapping(
        &self,
        expected_memory_size: u64,
    ) -> anyhow::Result<std::fs::File> {
        self.validate_memory_generation(expected_memory_size)?;
        let duplicate = self
            .memory_file
            .try_clone()
            .context("failed to duplicate snapshot memory handle")?;
        anyhow::ensure!(
            opened_file_generation(&duplicate, MEMORY_FILE_NAME)? == self.memory_generation,
            "snapshot memory duplicate does not refer to the opened generation",
        );
        Ok(duplicate)
    }

    /// Verifies that the opened memory generation and EOF are unchanged.
    pub fn validate_memory_generation(&self, expected_memory_size: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.manifest.memory_size_bytes == expected_memory_size,
            "memory.bin size in the manifest ({} bytes) doesn't match expected ({expected_memory_size} bytes)",
            self.manifest.memory_size_bytes,
        );
        anyhow::ensure!(
            opened_file_generation(&self.memory_file, MEMORY_FILE_NAME)? == self.memory_generation,
            "snapshot memory generation changed after it was opened",
        );
        Ok(())
    }

    /// Consumes this snapshot into decoded data and lifetime guard handles.
    pub fn into_parts(
        self,
    ) -> (
        SnapshotManifest,
        Vec<u8>,
        openvmm_defs::worker::SnapshotRestoreGuards,
    ) {
        (
            self.manifest,
            self.state_bytes,
            openvmm_defs::worker::SnapshotRestoreGuards {
                directory: self.directory.into_file(),
                manifest: self.manifest_file,
                state: self.state_file,
                memory: self.memory_file,
            },
        )
    }
}

fn read_snapshot_manifest_from_directory(
    directory: &OpenedSnapshotDirectory,
) -> anyhow::Result<(SnapshotManifest, std::fs::File)> {
    let manifest_file = directory.open_regular_file(MANIFEST_FILE_NAME, "snapshot manifest")?;
    let manifest = decode_snapshot_manifest(&manifest_file)?;
    Ok((manifest, manifest_file))
}

fn decode_snapshot_manifest(manifest_file: &std::fs::File) -> anyhow::Result<SnapshotManifest> {
    let manifest_bytes =
        read_bounded_open_file(manifest_file, MAX_MANIFEST_SIZE_BYTES, "snapshot manifest")?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(&manifest_bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_header(&manifest)?;
    validate_manifest_version(&manifest)?;
    Ok(manifest)
}

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and raw saved-state bytes after structurally
/// validating all three artifacts. `expected_memory_size` bounds memory.
pub fn read_snapshot(
    dir: &Path,
    expected_memory_size: u64,
) -> anyhow::Result<(SnapshotManifest, Vec<u8>)> {
    let snapshot = OpenedSnapshot::open(dir)?;
    snapshot.validate_memory_generation(expected_memory_size)?;
    let (manifest, state_bytes, _) = snapshot.into_parts();
    Ok((manifest, state_bytes))
}

/// Reads and structurally validates only `manifest.bin`.
///
/// Production restore should use [`OpenedSnapshot`] so later artifact access
/// remains anchored to the same opened directory generation.
pub fn read_snapshot_manifest(dir: &Path) -> anyhow::Result<SnapshotManifest> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    let (manifest, _) = read_snapshot_manifest_from_directory(&directory)?;
    validate_snapshot_directory(&directory)?;
    Ok(manifest)
}

/// Read and structurally validate a snapshot, returning its exact memory handle.
///
/// The returned file is positioned at offset zero and must be used directly
/// for restore so a path replacement cannot substitute different memory.
pub fn read_snapshot_with_memory(
    dir: &Path,
    expected_memory_size: u64,
) -> anyhow::Result<(SnapshotManifest, Vec<u8>, std::fs::File)> {
    let snapshot = OpenedSnapshot::open(dir)?;
    snapshot.validate_memory_generation(expected_memory_size)?;
    let (manifest, state_bytes, guards) = snapshot.into_parts();
    let openvmm_defs::worker::SnapshotRestoreGuards { memory, .. } = guards;
    Ok((manifest, state_bytes, memory))
}

/// Opens snapshot artifacts against an already validated manifest.
///
/// This compatibility helper reopens the directory. Production restore should
/// keep an [`OpenedSnapshot`] alive through worker construction instead.
pub fn read_snapshot_artifacts_with_memory(
    dir: &Path,
    manifest: &SnapshotManifest,
    expected_memory_size: u64,
) -> anyhow::Result<(Vec<u8>, std::fs::File)> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    validate_manifest_header(manifest)?;
    validate_manifest_version(manifest)?;
    validate_snapshot_directory(&directory)?;
    anyhow::ensure!(
        manifest.state_size_bytes <= MAX_SAVED_STATE_SIZE_BYTES,
        "state.bin length in the manifest exceeds the maximum size of \
         {MAX_SAVED_STATE_SIZE_BYTES} bytes",
    );

    let state_file = directory.open_regular_file(STATE_FILE_NAME, "saved state")?;
    let state_bytes =
        read_bounded_open_file(&state_file, MAX_SAVED_STATE_SIZE_BYTES, "saved state")?;
    anyhow::ensure!(
        state_bytes.len() as u64 == manifest.state_size_bytes,
        "state.bin size ({} bytes) doesn't match manifest ({} bytes)",
        state_bytes.len(),
        manifest.state_size_bytes,
    );
    anyhow::ensure!(
        manifest.memory_size_bytes == expected_memory_size,
        "memory.bin size in the manifest ({} bytes) doesn't match expected ({expected_memory_size} bytes)",
        manifest.memory_size_bytes,
    );
    let memory_file = directory.open_file_with_length(
        MEMORY_FILE_NAME,
        expected_memory_size,
        MEMORY_FILE_NAME,
    )?;

    Ok((state_bytes, memory_file))
}

fn validate_snapshot_directory(directory: &OpenedSnapshotDirectory) -> anyhow::Result<()> {
    let mut entries = HashSet::new();
    for name in directory.entry_names()? {
        anyhow::ensure!(
            name == MANIFEST_FILE_NAME || name == STATE_FILE_NAME || name == MEMORY_FILE_NAME,
            "unexpected artifact in snapshot directory: {}",
            directory.display_path(&name).display(),
        );
        entries.insert(name);
    }
    anyhow::ensure!(
        entries.len() == 3
            && entries.contains(std::ffi::OsStr::new(MANIFEST_FILE_NAME))
            && entries.contains(std::ffi::OsStr::new(STATE_FILE_NAME))
            && entries.contains(std::ffi::OsStr::new(MEMORY_FILE_NAME)),
        "snapshot directory is incomplete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::format::LEGACY_MANIFEST_VERSION;
    use super::super::format::LEGACY_SNAPSHOT_FORMAT_MAGIC;
    use super::super::format::SHA256_SIZE;
    use super::super::publish::write_snapshot;
    use super::super::tests::test_manifest;
    use super::*;
    use std::io::Read;
    #[cfg(target_os = "linux")]
    use std::io::Write;

    #[test]
    fn read_snapshot_accepts_same_length_state_change_without_legacy_checksum_validation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"other").unwrap();

        let (_, state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(state, b"other");
    }

    #[test]
    fn read_snapshot_accepts_same_length_memory_change_without_legacy_checksum_validation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1024]).unwrap();

        let (_, _, mut memory) = read_snapshot_with_memory(&snap_dir, 1024).unwrap();
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![1_u8; 1024]);
    }

    #[test]
    fn supplied_manifest_remains_authoritative_during_artifact_open() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let manifest = read_snapshot_manifest(&snap_dir).unwrap();

        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), b"replacement").unwrap();
        let (state, mut memory) =
            read_snapshot_artifacts_with_memory(&snap_dir, &manifest, 1024).unwrap();
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();

        assert_eq!(state, b"state");
        assert_eq!(bytes, vec![0_u8; 1024]);
    }

    #[test]
    fn read_snapshot_accepts_legacy_v2_manifest_without_verifying_digests() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.state_sha256 = vec![0xa5; SHA256_SIZE];
        manifest.memory_sha256 = vec![0x5a; SHA256_SIZE];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"other").unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1024]).unwrap();

        let (manifest, state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(manifest.version, LEGACY_MANIFEST_VERSION);
        assert_eq!(state, b"other");
    }

    #[test]
    fn read_snapshot_rejects_malformed_legacy_digest_lengths() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.state_sha256 = vec![0; SHA256_SIZE - 1];
        manifest.memory_sha256 = vec![0; SHA256_SIZE];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest.clone()),
        )
        .unwrap();
        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("state.bin SHA-256 digest"));

        manifest.state_sha256 = vec![0; SHA256_SIZE];
        manifest.memory_sha256 = vec![0; SHA256_SIZE + 1];
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();
        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("memory.bin SHA-256 digest"));
    }

    #[test]
    fn read_snapshot_still_requires_exact_memory_length() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MEMORY_FILE_NAME), vec![1_u8; 1023]).unwrap();

        let error = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(error.to_string().contains("memory.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_corrupt_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), b"not-a-manifest").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("decode snapshot manifest"));
    }

    #[test]
    fn read_snapshot_rejects_truncated_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(STATE_FILE_NAME), b"sta").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("state.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_truncated_memory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(MEMORY_FILE_NAME))
            .unwrap()
            .set_len(512)
            .unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("memory.bin size"));
    }

    #[test]
    fn read_snapshot_rejects_oversized_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(STATE_FILE_NAME))
            .unwrap()
            .set_len(MAX_SAVED_STATE_SIZE_BYTES + 1)
            .unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("saved state is"));
        assert!(err.to_string().contains("exceeding the maximum"));
    }

    #[test]
    fn read_snapshot_rejects_unexpected_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join("extra.bin"), b"unexpected").unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(err.to_string().contains("unexpected artifact"));
    }

    #[cfg(unix)]
    #[test]
    fn read_snapshot_rejects_symlinked_artifact() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let state_path = snap_dir.join(STATE_FILE_NAME);
        let moved_state_path = snap_dir.join("state-target.bin");
        std::fs::rename(&state_path, &moved_state_path).unwrap();
        symlink(&moved_state_path, &state_path).unwrap();

        let err = read_snapshot(&snap_dir, 1024).err().unwrap();
        assert!(
            err.to_string().contains("unexpected artifact")
                || err.to_string().contains("not a regular file")
                || err.to_string().contains("failed to open saved state")
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_memory_handle_survives_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0x5a_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();

        let (_, _, mut opened_memory) = read_snapshot_with_memory(&snap_dir, 1024).unwrap();
        let original_path = snap_dir.join(MEMORY_FILE_NAME);
        let moved_path = snap_dir.join("opened-memory.bin");
        std::fs::rename(&original_path, &moved_path).unwrap();
        std::fs::write(&original_path, vec![0xa5_u8; 1024]).unwrap();

        let mut bytes = Vec::new();
        opened_memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0x5a_u8; 1024]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opened_snapshot_survives_directory_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let moved_dir = dir.path().join("opened-snapshot");
        let memory_source = dir.path().join("memory-source.bin");
        std::fs::write(&memory_source, vec![0x5a_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        std::fs::rename(&snap_dir, &moved_dir).unwrap();
        std::fs::write(&memory_source, vec![0xa5_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"other", &memory_source).unwrap();

        snapshot.validate_memory_generation(1024).unwrap();
        let (_, state, guards) = snapshot.into_parts();
        let mut memory = guards.memory;
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(state, b"state");
        assert_eq!(bytes, vec![0x5a_u8; 1024]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn opened_snapshot_detects_same_length_memory_write_before_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_source = dir.path().join("memory-source.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .open(snap_dir.join(MEMORY_FILE_NAME))
            .unwrap();
        writer.write_all(&[1]).unwrap();
        writer.sync_all().unwrap();

        let error = snapshot
            .duplicate_memory_file_for_mapping(1024)
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("memory generation changed after it was opened")
        );
    }

    #[cfg(windows)]
    #[test]
    fn opened_snapshot_denies_mutation_until_guards_drop() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let moved_dir = dir.path().join("moved");
        let memory_source = dir.path().join("memory-source.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        let memory_path = snap_dir.join(MEMORY_FILE_NAME);
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .open(&memory_path)
                .is_err()
        );
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&memory_path)
                .is_err()
        );
        assert!(std::fs::remove_file(&memory_path).is_err());
        assert!(std::fs::rename(&snap_dir, &moved_dir).is_err());

        drop(snapshot);
        std::fs::rename(&snap_dir, &moved_dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn opened_snapshot_rejects_reparse_artifact() {
        use std::os::windows::fs::symlink_file;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_source = dir.path().join("memory-source.bin");
        let replacement = dir.path().join("replacement.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &memory_source).unwrap();
        std::fs::rename(snap_dir.join(MEMORY_FILE_NAME), &replacement).unwrap();
        match symlink_file(&replacement, snap_dir.join(MEMORY_FILE_NAME)) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314) =>
            {
                return;
            }
            Err(error) => panic!("failed to create test symlink: {error}"),
        }

        let error = OpenedSnapshot::open(&snap_dir).err().unwrap();
        assert!(
            error.to_string().contains("reparse point")
                || error.to_string().contains("not a regular file")
        );
    }
}
