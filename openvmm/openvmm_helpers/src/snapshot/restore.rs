// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Restore-side snapshot access: opening one published generation through a
//! retained directory handle, structural validation against its manifest,
//! paired scratch materialization, and single-use resume claims.

use super::SnapshotManifest;
use super::format::MANIFEST_FILE_NAME;
use super::format::MAX_MANIFEST_SIZE_BYTES;
use super::format::MAX_SAVED_STATE_SIZE_BYTES;
use super::format::MEMORY_FILE_NAME;
use super::format::RESUME_CLAIM_FILE_NAME;
use super::format::SCRATCH_FILE_NAME;
use super::format::SNAPSHOT_RESTORE_POLICY_RESUME;
use super::format::STATE_FILE_NAME;
use super::format::decode_manifest;
use super::format::validate_manifest_contents;
use super::format::validate_manifest_header;
use super::fs::OpenedFileGeneration;
use super::fs::OpenedSnapshotDirectory;
use super::fs::allocated_file_bytes;
use super::fs::copy_exact;
use super::fs::create_hard_link_from_handle;
use super::fs::open_file_with_length;
use super::fs::opened_file_generation;
use super::fs::read_bounded_open_file;
use super::fs::reflink_exact;
use super::fs::verify_hard_link_identity;
use super::microvm;
use anyhow::Context;
use std::collections::HashSet;
use std::io::Write;
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
    ///
    /// The paired scratch, if any, must be a regular file of its recorded
    /// length, as [`Self::open_paired_scratch_file`] requires, so that a
    /// restore never claims a resume snapshot whose scratch it cannot open.
    pub fn open(dir: &Path) -> anyhow::Result<Self> {
        let directory = OpenedSnapshotDirectory::open(dir)?;
        let manifest_file = directory.open_regular_file(MANIFEST_FILE_NAME, "snapshot manifest")?;
        let state_file = directory.open_regular_file(STATE_FILE_NAME, "saved state")?;
        let memory_file = directory.open_regular_file(MEMORY_FILE_NAME, "snapshot memory")?;
        let manifest = decode_snapshot_manifest(&manifest_file)?;
        validate_snapshot_directory(&directory, &manifest)?;
        // The handle is not kept: restore opens the paired scratch again to
        // materialize it, and on Windows a held handle, which shares only
        // reads, would refuse the writer of a direct-claimed scratch.
        open_paired_scratch_file_in_directory(&directory, &manifest)?;
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

    /// Opens the paired scratch artifact relative to this snapshot generation.
    pub fn open_paired_scratch_file(&self) -> anyhow::Result<Option<std::fs::File>> {
        open_paired_scratch_file_in_directory(&self.directory, &self.manifest)
    }

    /// Claims this exact opened generation for a single-use resume.
    pub fn claim_for_restore(&self) -> anyhow::Result<()> {
        claim_snapshot_for_restore_in_directory(&self.directory, &self.manifest)
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
    let manifest = decode_manifest(&manifest_bytes)?;
    validate_manifest_contents(&manifest)?;
    if let Some(contract) = &manifest.machine_contract {
        microvm::validate_machine_contract_shape(
            contract,
            manifest.memory_size_bytes,
            manifest.vp_count,
        )?;
    }
    Ok(manifest)
}

/// Read a snapshot from the given directory.
///
/// Returns the decoded manifest and raw saved-state bytes after structurally
/// validating its artifacts. `expected_memory_size` bounds memory.
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
    validate_snapshot_directory(&directory, &manifest)?;
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

/// Opens the scratch image paired to a snapshot, if present, which must have
/// its recorded length. Its storage generation vouches for its content, which
/// the restore never reads to verify.
pub fn open_paired_scratch_file(
    dir: &Path,
    manifest: &SnapshotManifest,
) -> anyhow::Result<Option<std::fs::File>> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    open_paired_scratch_file_in_directory(&directory, manifest)
}

fn open_paired_scratch_file_in_directory(
    directory: &OpenedSnapshotDirectory,
    manifest: &SnapshotManifest,
) -> anyhow::Result<Option<std::fs::File>> {
    let Some(scratch) = microvm::paired_scratch_block(manifest) else {
        return Ok(None);
    };
    let file =
        directory.open_file_with_length(SCRATCH_FILE_NAME, scratch.length, SCRATCH_FILE_NAME)?;
    Ok(Some(file))
}

/// Materializes paired scratch as an independent private file.
pub fn copy_paired_scratch_file(
    source: &std::fs::File,
    destination: &Path,
    block: &microvm::SnapshotMicrovmSandboxBlock,
) -> anyhow::Result<()> {
    copy_exact(
        source,
        destination,
        block.length,
        SCRATCH_FILE_NAME,
        "private scratch copy",
    )
}

/// Materializes paired scratch with a required filesystem copy-on-write clone.
pub fn clone_paired_scratch_file(
    source: &std::fs::File,
    destination: &Path,
    block: &microvm::SnapshotMicrovmSandboxBlock,
) -> anyhow::Result<()> {
    reflink_exact(
        source,
        destination,
        block.length,
        SCRATCH_FILE_NAME,
        "copy-on-write scratch clone",
    )
}

/// Creates a private path to the exact claimed paired-scratch generation.
pub fn link_claimed_paired_scratch_file(
    source: &std::fs::File,
    destination: &Path,
    block: &microvm::SnapshotMicrovmSandboxBlock,
) -> anyhow::Result<()> {
    let parent = destination
        .parent()
        .context("claimed scratch destination has no parent")?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .context("claimed scratch destination name is invalid")?;
    let directory = OpenedSnapshotDirectory::open_for_publication(parent)?;
    create_hard_link_from_handle(source, &directory.file, name)
        .context("failed to link claimed scratch generation")?;
    let linked = open_file_with_length(destination, block.length, "claimed scratch link")?;
    verify_hard_link_identity(source, &linked, block.length)
}

/// Atomically consumes a single-use resume snapshot.
///
/// Call this after artifact and configuration validation and before constructing
/// execution-owned workers. A later startup failure does not roll back the claim.
/// Clone snapshots are unchanged.
pub fn claim_snapshot_for_restore(dir: &Path, manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let directory = OpenedSnapshotDirectory::open(dir)?;
    claim_snapshot_for_restore_in_directory(&directory, manifest)
}

fn claim_snapshot_for_restore_in_directory(
    directory: &OpenedSnapshotDirectory,
    manifest: &SnapshotManifest,
) -> anyhow::Result<()> {
    validate_manifest_header(manifest)?;
    validate_manifest_contents(manifest)?;
    if manifest.restore_policy != SNAPSHOT_RESTORE_POLICY_RESUME {
        return Ok(());
    }

    let mut claim = match directory.create_new_file(RESUME_CLAIM_FILE_NAME) {
        Ok(claim) => claim,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            anyhow::bail!("resume snapshot has already been claimed")
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to claim resume snapshot at {}",
                    directory.display_path(RESUME_CLAIM_FILE_NAME).display()
                )
            });
        }
    };
    claim
        .write_all(b"OPENVMM_RESUME_CLAIM_V1\n")
        .context("failed to write resume snapshot claim")?;
    claim
        .sync_all()
        .context("failed to flush resume snapshot claim")?;
    directory
        .sync()
        .context("failed to commit resume snapshot claim")?;
    Ok(())
}

fn validate_snapshot_directory(
    directory: &OpenedSnapshotDirectory,
    manifest: &SnapshotManifest,
) -> anyhow::Result<()> {
    let has_scratch = microvm::paired_scratch_block(manifest).is_some();
    let mut entries = HashSet::new();
    for name in directory.entry_names()? {
        if name == RESUME_CLAIM_FILE_NAME
            && manifest.restore_policy == SNAPSHOT_RESTORE_POLICY_RESUME
        {
            anyhow::bail!("resume snapshot has already been claimed");
        }
        anyhow::ensure!(
            name == MANIFEST_FILE_NAME
                || name == STATE_FILE_NAME
                || name == MEMORY_FILE_NAME
                || (has_scratch && name == SCRATCH_FILE_NAME),
            "unexpected artifact in snapshot directory: {}",
            directory.display_path(&name).display(),
        );
        entries.insert(name);
    }
    anyhow::ensure!(
        entries.len() == 3 + usize::from(has_scratch)
            && entries.contains(std::ffi::OsStr::new(MANIFEST_FILE_NAME))
            && entries.contains(std::ffi::OsStr::new(STATE_FILE_NAME))
            && entries.contains(std::ffi::OsStr::new(MEMORY_FILE_NAME))
            && (!has_scratch || entries.contains(std::ffi::OsStr::new(SCRATCH_FILE_NAME))),
        "snapshot directory is incomplete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::format::SNAPSHOT_RESTORE_POLICY_CLONE;
    use super::super::format::SNAPSHOT_TIER_INSTANCE_CHECKPOINT;
    use super::super::format::SNAPSHOT_TIER_WORKLOAD_START;
    #[cfg(target_os = "linux")]
    use super::super::fs::initialize_snapshot_memory_backing_file;
    use super::super::microvm::paired_scratch_manifest;
    use super::super::microvm::test_machine_contract;
    use super::super::publish::write_snapshot;
    use super::super::publish::write_snapshot_from_memory_and_scratch_files;
    #[cfg(target_os = "linux")]
    use super::super::publish::write_snapshot_from_owned_memory_and_scratch_files;
    use super::super::tests::test_manifest;
    use super::*;
    use mesh::payload::Protobuf;
    use mesh::payload::Timestamp;
    use openvmm_defs::microvm::MICROVM_COMMAND_LINE_MAX_SIZE;
    use openvmm_defs::microvm::check_microvm_command_line;
    use sha2::Digest;
    use std::io::Read;
    #[cfg(any(target_os = "linux", windows))]
    use std::io::Seek;
    #[cfg(any(target_os = "linux", windows))]
    use std::io::SeekFrom;
    #[cfg(any(target_os = "linux", windows))]
    use std::io::Write;

    #[test]
    fn private_scratch_copy_is_exact_and_independent() {
        let manifest = paired_scratch_manifest(&[0x5a; 512]);
        let block = microvm::paired_scratch_block(&manifest).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source-scratch.img");
        let destination_path = dir.path().join("private-scratch.img");
        // The storage generation vouches for the content, which the copy takes
        // as it is, without reading it to verify it.
        std::fs::write(&source_path, vec![0xa5; 512]).unwrap();
        let source = std::fs::File::open(&source_path).unwrap();

        copy_paired_scratch_file(&source, &destination_path, block).unwrap();
        assert_eq!(std::fs::read(&destination_path).unwrap(), vec![0xa5; 512]);
        std::fs::write(&destination_path, vec![0x3c; 512]).unwrap();
        assert_eq!(std::fs::read(&source_path).unwrap(), vec![0xa5; 512]);

        // A source of another length is refused.
        let longer_path = dir.path().join("longer-scratch.img");
        std::fs::write(&longer_path, vec![0xa5; 1024]).unwrap();
        let error = copy_paired_scratch_file(
            &std::fs::File::open(&longer_path).unwrap(),
            &dir.path().join("refused-scratch.img"),
            block,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("doesn't match manifest"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copy_on_write_scratch_clone_is_independent() {
        let scratch = vec![0x5a; 512];
        let manifest = paired_scratch_manifest(&scratch);
        let block = microvm::paired_scratch_block(&manifest).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source-scratch.img");
        let destination_path = dir.path().join("cloned-scratch.img");
        std::fs::write(&source_path, &scratch).unwrap();
        let source = std::fs::File::open(&source_path).unwrap();

        if let Err(error) = clone_paired_scratch_file(&source, &destination_path, block) {
            let error = format!("{error:#}");
            assert!(
                error.contains("requires filesystem reflink support"),
                "{error}"
            );
            return;
        }

        let mut clone = std::fs::OpenOptions::new()
            .write(true)
            .open(&destination_path)
            .unwrap();
        clone.write_all(b"clone").unwrap();
        clone.sync_all().unwrap();

        assert_eq!(&std::fs::read(&source_path).unwrap()[..5], b"ZZZZZ");
        assert_eq!(&std::fs::read(&destination_path).unwrap()[..5], b"clone");
    }

    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn direct_claimed_scratch_links_exact_claimed_generation() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest.snapshot_tier = SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_RESUME.to_owned();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line(
                "console=hvc0 nvx_snapshot_tier=instance-checkpoint".to_owned(),
            );
        let block = microvm::paired_scratch_block(&manifest).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let snapshot_dir = dir.path().join("snapshot");
        let claimed_dir = dir.path().join("claimed");
        std::fs::create_dir(&snapshot_dir).unwrap();
        std::fs::create_dir(&claimed_dir).unwrap();
        let source_path = snapshot_dir.join(SCRATCH_FILE_NAME);
        let destination_path = claimed_dir.join(SCRATCH_FILE_NAME);
        std::fs::write(&source_path, &scratch).unwrap();
        let source = std::fs::File::open(&source_path).unwrap();

        claim_snapshot_for_restore(&snapshot_dir, &manifest).unwrap();
        assert!(
            claim_snapshot_for_restore(&snapshot_dir, &manifest)
                .unwrap_err()
                .to_string()
                .contains("already been claimed")
        );
        link_claimed_paired_scratch_file(&source, &destination_path, block).unwrap();

        let mut claimed = std::fs::OpenOptions::new()
            .write(true)
            .open(&destination_path)
            .unwrap();
        claimed.seek(SeekFrom::Start(0)).unwrap();
        claimed.write_all(b"claim").unwrap();
        claimed.sync_all().unwrap();

        assert_eq!(&std::fs::read(&source_path).unwrap()[..5], b"claim");
    }

    #[test]
    fn resume_snapshot_claim_is_single_use() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest.snapshot_tier = SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_RESUME.to_owned();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line(
                "console=hvc0 nvx_snapshot_tier=instance-checkpoint".to_owned(),
            );
        let dir = tempfile::tempdir().unwrap();

        claim_snapshot_for_restore(dir.path(), &manifest).unwrap();
        let error = claim_snapshot_for_restore(dir.path(), &manifest).unwrap_err();
        assert!(error.to_string().contains("already been claimed"));
    }

    #[test]
    fn clone_snapshot_claim_is_a_noop() {
        let scratch = vec![0x5a; 512];
        let manifest = paired_scratch_manifest(&scratch);
        let dir = tempfile::tempdir().unwrap();

        claim_snapshot_for_restore(dir.path(), &manifest).unwrap();
        claim_snapshot_for_restore(dir.path(), &manifest).unwrap();
        assert!(!dir.path().join(RESUME_CLAIM_FILE_NAME).exists());
    }

    #[test]
    fn read_snapshot_accepts_same_length_state_change_without_checksums() {
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
    fn read_snapshot_accepts_same_length_memory_change_without_checksums() {
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
    fn read_snapshot_rejects_another_manifest_version() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        manifest.version = 1;
        manifest.format_magic = b"OPENVMM_SNAPSHOT_V1\0".to_vec();
        std::fs::write(
            snap_dir.join(MANIFEST_FILE_NAME),
            mesh::payload::encode(manifest),
        )
        .unwrap();

        let error = format!("{:#}", read_snapshot(&snap_dir, 1024).err().unwrap());
        assert!(error.contains("[E_SNAPSHOT_VERSION]"), "{error}");
        assert!(error.contains("recapture the snapshot"), "{error}");
    }

    /// Returns the errors of reading a snapshot whole and its manifest alone,
    /// once `manifest` replaces the manifest of a snapshot of this format.
    fn read_errors_with_manifest(manifest: &[u8]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), manifest).unwrap();
        [
            read_snapshot(&snap_dir, 1024).err().unwrap(),
            read_snapshot_manifest(&snap_dir).err().unwrap(),
        ]
        .iter()
        .map(|error| format!("{error:#}"))
        .collect()
    }

    #[test]
    fn read_snapshot_refuses_a_version_6_manifest_by_its_version() {
        // The manifest of earlier OpenVMM releases, version 6, gives fields 11
        // and 13 other types than this format does: its machine contract, a
        // message, is where this format keeps the saved-state schema version,
        // a varint, and its schema version where this format keeps the capture
        // tier, a string.
        #[derive(Protobuf)]
        #[mesh(package = "openvmm.snapshot")]
        struct ManifestV6 {
            #[mesh(1)]
            version: u32,
            #[mesh(2)]
            created_at: Timestamp,
            #[mesh(3)]
            openvmm_version: String,
            #[mesh(4)]
            memory_size_bytes: u64,
            #[mesh(5)]
            vp_count: u32,
            #[mesh(6)]
            page_size: u32,
            #[mesh(7)]
            architecture: String,
            #[mesh(8)]
            state_size_bytes: u64,
            #[mesh(11)]
            machine_contract: Option<microvm::SnapshotMachineContract>,
            #[mesh(12)]
            format_magic: Vec<u8>,
            #[mesh(13)]
            saved_state_schema_version: u32,
            #[mesh(14)]
            saved_state_root_type: String,
            #[mesh(15)]
            snapshot_tier: String,
            #[mesh(16)]
            restore_policy: String,
            #[mesh(17)]
            consumed_config_sections: u32,
        }

        let manifest = mesh::payload::encode(ManifestV6 {
            version: 6,
            created_at: Timestamp {
                seconds: 1234567890,
                nanos: 0,
            },
            openvmm_version: "test-0.1.0".to_owned(),
            memory_size_bytes: 1024,
            vp_count: 2,
            page_size: 4096,
            architecture: "x86_64".to_owned(),
            state_size_bytes: 5,
            machine_contract: Some(test_machine_contract()),
            format_magic: b"OPENVMM_SNAPSHOT_V6\0".to_vec(),
            saved_state_schema_version: 1,
            saved_state_root_type: "openvmm.SavedState".to_owned(),
            snapshot_tier: "platform".to_owned(),
            restore_policy: "clone".to_owned(),
            consumed_config_sections: 1,
        });
        for error in read_errors_with_manifest(&manifest) {
            assert!(error.contains("[E_SNAPSHOT_VERSION]"), "{error}");
            assert!(
                error.contains("snapshot manifest version 6 is not supported"),
                "{error}"
            );
            assert!(error.contains("recapture the snapshot"), "{error}");
        }
    }

    #[test]
    fn read_snapshot_refuses_an_early_version_2_manifest_by_its_magic() {
        // The first manifest version 2, which predates the format magic, kept
        // the SHA-256 digests of `state.bin` and `memory.bin` in fields 9 and
        // 10, where this format keeps its machine contract and its magic.
        #[derive(Protobuf)]
        #[mesh(package = "openvmm.snapshot")]
        struct ManifestV2WithDigests {
            #[mesh(1)]
            version: u32,
            #[mesh(2)]
            created_at: Timestamp,
            #[mesh(3)]
            openvmm_version: String,
            #[mesh(4)]
            memory_size_bytes: u64,
            #[mesh(5)]
            vp_count: u32,
            #[mesh(6)]
            page_size: u32,
            #[mesh(7)]
            architecture: String,
            #[mesh(8)]
            state_size_bytes: u64,
            #[mesh(9)]
            state_sha256: Vec<u8>,
            #[mesh(10)]
            memory_sha256: Vec<u8>,
        }

        let manifest = mesh::payload::encode(ManifestV2WithDigests {
            version: 2,
            created_at: Timestamp {
                seconds: 1234567890,
                nanos: 0,
            },
            openvmm_version: "test-0.1.0".to_owned(),
            memory_size_bytes: 1024,
            vp_count: 2,
            page_size: 4096,
            architecture: "x86_64".to_owned(),
            state_size_bytes: 5,
            state_sha256: sha2::Sha256::digest(b"state").to_vec(),
            memory_sha256: sha2::Sha256::digest([0_u8; 1024]).to_vec(),
        });
        for error in read_errors_with_manifest(&manifest) {
            assert!(error.contains("[E_SNAPSHOT_VERSION]"), "{error}");
            assert!(
                error.contains("snapshot format magic is invalid"),
                "{error}"
            );
            assert!(error.contains("recapture the snapshot"), "{error}");
        }
    }

    #[test]
    fn read_snapshot_rejects_a_command_line_over_the_limit_without_rewriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        // A snapshot whose saved command line no longer fits, as one captured
        // under a larger limit would record: one byte over the limit.
        let mut manifest = read_snapshot_manifest(&snap_dir).unwrap();
        let mut contract = test_machine_contract();
        let prefix = "console=hvc0 ";
        let fill = MICROVM_COMMAND_LINE_MAX_SIZE
            .checked_sub(prefix.len())
            .expect("the prefix fits within the limit");
        let saved = format!("{prefix}{}", "x".repeat(fill));
        // The shared check's error, whose text openvmm_defs tests.
        let expected = format!(
            "snapshot effective command line is invalid: {}",
            check_microvm_command_line(&saved).unwrap_err()
        );
        contract.set_effective_command_line(saved);
        manifest.machine_contract = Some(contract);
        let encoded = mesh::payload::encode(manifest);
        std::fs::write(snap_dir.join(MANIFEST_FILE_NAME), &encoded).unwrap();
        let entries = || {
            let mut names = std::fs::read_dir(&snap_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>();
            names.sort();
            names
        };
        let before = entries();

        for error in [
            read_snapshot(&snap_dir, 1024).err().unwrap(),
            read_snapshot_manifest(&snap_dir).err().unwrap(),
        ] {
            let error = format!("{error:#}");
            assert!(error.contains(&expected), "{error}");
        }
        // The restore neither rewrites the saved string nor migrates the snapshot.
        assert_eq!(
            std::fs::read(snap_dir.join(MANIFEST_FILE_NAME)).unwrap(),
            encoded
        );
        assert_eq!(entries(), before);
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

    /// A paired scratch restore mode that this OpenVMM does not know, which a
    /// corrupted or crafted manifest, or one from a newer OpenVMM, can record,
    /// fails the open of a clone or resume snapshot with the shape check's
    /// error. The capture-tier check, which matches the restore mode, runs
    /// before the shape check on this path. A claim refuses it the same way
    /// and leaves the snapshot unclaimed.
    #[test]
    fn unknown_paired_scratch_restore_mode_is_refused() {
        let scratch = vec![0x5a; 512];
        let state = b"state";
        let expected = "snapshot scratch restore mode 'a-future-restore-mode' is unsupported";
        for (tier, policy) in [
            (SNAPSHOT_TIER_WORKLOAD_START, SNAPSHOT_RESTORE_POLICY_CLONE),
            (
                SNAPSHOT_TIER_INSTANCE_CHECKPOINT,
                SNAPSHOT_RESTORE_POLICY_RESUME,
            ),
        ] {
            let mut manifest = paired_scratch_manifest(&scratch);
            manifest.state_size_bytes = state.len() as u64;
            manifest.snapshot_tier = tier.to_owned();
            manifest.restore_policy = policy.to_owned();
            let contract = manifest.machine_contract.as_mut().unwrap();
            contract.set_effective_command_line(format!("console=hvc0 nvx_snapshot_tier={tier}"));
            let dir = tempfile::tempdir().unwrap();
            let manifest_path = dir.path().join(MANIFEST_FILE_NAME);
            std::fs::write(&manifest_path, mesh::payload::encode(manifest.clone())).unwrap();
            std::fs::write(dir.path().join(STATE_FILE_NAME), state).unwrap();
            std::fs::write(
                dir.path().join(MEMORY_FILE_NAME),
                vec![0_u8; manifest.memory_size_bytes as usize],
            )
            .unwrap();
            std::fs::write(dir.path().join(SCRATCH_FILE_NAME), &scratch).unwrap();
            // The snapshot opens with the restore mode that it recorded.
            drop(OpenedSnapshot::open(dir.path()).unwrap());

            let contract = manifest.machine_contract.as_mut().unwrap();
            contract
                .microvm_sandbox_blocks
                .last_mut()
                .unwrap()
                .restore_mode = "a-future-restore-mode".to_owned();
            std::fs::write(&manifest_path, mesh::payload::encode(manifest.clone())).unwrap();
            let error = OpenedSnapshot::open(dir.path()).err().unwrap();
            assert_eq!(error.to_string(), expected, "{tier}");
            let shape_error = microvm::validate_machine_contract_shape(
                manifest.machine_contract.as_ref().unwrap(),
                manifest.memory_size_bytes,
                manifest.vp_count,
            )
            .unwrap_err();
            assert_eq!(shape_error.to_string(), expected, "{tier}");
            let claim_error = claim_snapshot_for_restore(dir.path(), &manifest).unwrap_err();
            assert_eq!(claim_error.to_string(), expected, "{tier}");
            assert!(!dir.path().join(RESUME_CLAIM_FILE_NAME).exists(), "{tier}");
        }
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

    /// Restore refuses a FIFO in place of any artifact of a paired-scratch
    /// snapshot for what it is, without waiting for the writer that a
    /// blocking open of the FIFO would wait for.
    #[cfg(target_os = "linux")]
    #[test]
    fn restore_refuses_a_writerless_fifo_artifact_without_waiting() {
        use super::super::fs::tests::refusal_of_writerless_fifo;

        let fifo_mode = nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR;
        let scratch = vec![0x5a_u8; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest.snapshot_tier = SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_RESUME.to_owned();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line(
                "console=hvc0 nvx_snapshot_tier=instance-checkpoint".to_owned(),
            );
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_source = dir.path().join("memory-source.bin");
        let scratch_source = dir.path().join("scratch-source.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        std::fs::write(&scratch_source, &scratch).unwrap();
        write_snapshot_from_memory_and_scratch_files(
            &snap_dir,
            &manifest,
            b"state",
            &std::fs::File::open(&memory_source).unwrap(),
            Some(&std::fs::File::open(&scratch_source).unwrap()),
        )
        .unwrap();

        for name in [MANIFEST_FILE_NAME, STATE_FILE_NAME, MEMORY_FILE_NAME] {
            let artifact = snap_dir.join(name);
            let set_aside = dir.path().join(name);
            std::fs::rename(&artifact, &set_aside).unwrap();
            nix::unistd::mkfifo(&artifact, fifo_mode).unwrap();
            let snapshot = snap_dir.clone();
            let error = refusal_of_writerless_fifo(&artifact, move || {
                OpenedSnapshot::open(&snapshot).map(drop)
            });
            assert!(
                format!("{error:#}").contains("is not a regular file"),
                "{name}: {error:#}"
            );
            std::fs::remove_file(&artifact).unwrap();
            std::fs::rename(&set_aside, &artifact).unwrap();
        }

        // Restore opens the paired scratch again to materialize it, once it
        // has opened the snapshot.
        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        let scratch_path = snap_dir.join(SCRATCH_FILE_NAME);
        std::fs::remove_file(&scratch_path).unwrap();
        nix::unistd::mkfifo(&scratch_path, fifo_mode).unwrap();
        let error = refusal_of_writerless_fifo(&scratch_path, move || {
            snapshot.open_paired_scratch_file().map(drop)
        });
        assert!(
            format!("{error:#}").contains("scratch.img is not a regular file"),
            "{error:#}"
        );
    }

    /// The open of a resume snapshot refuses a paired `scratch.img` that is
    /// not a regular file of its recorded length, so a restore that claims
    /// the snapshot once it has opened it never consumes a snapshot whose
    /// scratch it cannot open. Once `scratch.img` is the published file again,
    /// the snapshot opens, and the claim takes it.
    #[test]
    fn paired_scratch_of_another_type_or_length_is_refused_before_a_claim() {
        let scratch = vec![0x5a_u8; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest.snapshot_tier = SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_RESUME.to_owned();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line(
                "console=hvc0 nvx_snapshot_tier=instance-checkpoint".to_owned(),
            );
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let memory_source = dir.path().join("memory-source.bin");
        let scratch_source = dir.path().join("scratch-source.bin");
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        std::fs::write(&scratch_source, &scratch).unwrap();
        write_snapshot_from_memory_and_scratch_files(
            &snap_dir,
            &manifest,
            b"state",
            &std::fs::File::open(&memory_source).unwrap(),
            Some(&std::fs::File::open(&scratch_source).unwrap()),
        )
        .unwrap();
        let scratch_path = snap_dir.join(SCRATCH_FILE_NAME);
        let published = dir.path().join(SCRATCH_FILE_NAME);
        let claim_path = snap_dir.join(RESUME_CLAIM_FILE_NAME);
        std::fs::rename(&scratch_path, &published).unwrap();
        let refused = |case: &str, expected: &str| {
            let Err(error) =
                OpenedSnapshot::open(&snap_dir).and_then(|snapshot| snapshot.claim_for_restore())
            else {
                panic!("{case}: the snapshot opened, and the claim consumed it");
            };
            assert!(format!("{error:#}").contains(expected), "{case}: {error:#}");
            assert!(!claim_path.exists(), "{case}");
        };

        // A directory, which the open on Windows refuses to open as a file,
        // and elsewhere opens and refuses for its type.
        std::fs::create_dir(&scratch_path).unwrap();
        refused(
            "directory",
            if cfg!(windows) {
                "failed to open scratch.img"
            } else {
                "scratch.img is not a regular file"
            },
        );
        std::fs::remove_dir(&scratch_path).unwrap();

        // A symbolic link to the published scratch, which the open does not
        // follow: on Unix, it fails to open it, and on Windows, it opens the
        // link itself and refuses its type.
        #[cfg(unix)]
        std::os::unix::fs::symlink(&published, &scratch_path).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&published, &scratch_path) {
            // Creating a symbolic link takes a privilege that the test may lack.
            assert!(
                error.kind() == std::io::ErrorKind::PermissionDenied
                    || error.raw_os_error() == Some(1314),
                "failed to create test symlink: {error}"
            );
        }
        if scratch_path.symlink_metadata().is_ok() {
            refused(
                "symbolic link",
                if cfg!(windows) {
                    "scratch.img is not a regular file"
                } else {
                    "failed to open scratch.img"
                },
            );
            std::fs::remove_file(&scratch_path).unwrap();
        }

        // A FIFO without a writer, which the open refuses without waiting.
        #[cfg(target_os = "linux")]
        {
            use super::super::fs::tests::refusal_of_writerless_fifo;

            nix::unistd::mkfifo(
                &scratch_path,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
            let snapshot = snap_dir.clone();
            let error = refusal_of_writerless_fifo(&scratch_path, move || {
                OpenedSnapshot::open(&snapshot)?.claim_for_restore()
            });
            assert!(
                format!("{error:#}").contains("scratch.img is not a regular file"),
                "{error:#}"
            );
            assert!(!claim_path.exists());
            std::fs::remove_file(&scratch_path).unwrap();
        }

        // A regular file of another length.
        std::fs::write(&scratch_path, vec![0x5a_u8; 1024]).unwrap();
        refused(
            "longer file",
            "scratch.img size (1024 bytes) doesn't match manifest (512 bytes)",
        );
        std::fs::remove_file(&scratch_path).unwrap();

        // The published scratch again.
        std::fs::rename(&published, &scratch_path).unwrap();
        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        snapshot.claim_for_restore().unwrap();
        assert!(claim_path.exists());
        snapshot.open_paired_scratch_file().unwrap().unwrap();
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
    fn resume_claim_targets_opened_directory_generation() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let moved_dir = dir.path().join("opened-snapshot");
        let memory_source = dir.path().join("memory-source.bin");
        let scratch_source = dir.path().join("scratch-source.bin");
        let scratch = vec![0x5a_u8; 512];
        std::fs::write(&memory_source, vec![0_u8; 1024]).unwrap();
        std::fs::write(&scratch_source, &scratch).unwrap();
        let memory_file = std::fs::File::open(&memory_source).unwrap();
        let scratch_file = std::fs::File::open(&scratch_source).unwrap();
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest.snapshot_tier = SNAPSHOT_TIER_INSTANCE_CHECKPOINT.to_owned();
        manifest.restore_policy = SNAPSHOT_RESTORE_POLICY_RESUME.to_owned();
        manifest
            .machine_contract
            .as_mut()
            .unwrap()
            .set_effective_command_line(
                "console=hvc0 nvx_snapshot_tier=instance-checkpoint".to_owned(),
            );
        write_snapshot_from_memory_and_scratch_files(
            &snap_dir,
            &manifest,
            b"state",
            &memory_file,
            Some(&scratch_file),
        )
        .unwrap();

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        std::fs::rename(&snap_dir, &moved_dir).unwrap();
        std::fs::create_dir(&snap_dir).unwrap();
        snapshot.claim_for_restore().unwrap();

        assert!(moved_dir.join(RESUME_CLAIM_FILE_NAME).exists());
        assert!(!snap_dir.join(RESUME_CLAIM_FILE_NAME).exists());
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

    /// A capture with automatic RAM publishes `memory.bin` as a hard link of
    /// the RAM file that the source created beside the destination, and the
    /// source removes that file's temporary name when it tears down, after the
    /// commit. The removal changes `memory.bin`'s change time, but not its
    /// identity, length, modification time, or content, so a restore that
    /// opened the snapshot before the removal still maps the memory that it
    /// opened.
    #[cfg(target_os = "linux")]
    #[test]
    fn source_ram_removal_after_open_keeps_the_memory_generation() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let ram = tempfile::Builder::new()
            .prefix(".openvmm-microvm-memory-")
            .tempfile_in(dir.path())
            .unwrap();
        initialize_snapshot_memory_backing_file(ram.as_file(), 1024).unwrap();
        let mut ram_handle = ram.reopen().unwrap();
        ram_handle.write_all(&[0x5a_u8; 1024]).unwrap();
        write_snapshot_from_owned_memory_and_scratch_files(
            &snap_dir,
            &test_manifest(),
            b"state",
            &ram_handle,
            None,
        )
        .unwrap();
        let memory_path = snap_dir.join(MEMORY_FILE_NAME);
        let published = std::fs::metadata(&memory_path).unwrap();
        assert_eq!(published.ino(), ram.as_file().metadata().unwrap().ino());
        assert_eq!(published.nlink(), 2);

        let snapshot = OpenedSnapshot::open(&snap_dir).unwrap();
        // Let the clock pass the recorded times, even on a file system that
        // keeps them in whole seconds.
        std::thread::sleep(std::time::Duration::from_secs(1));
        drop(ram);

        let unlinked = std::fs::metadata(&memory_path).unwrap();
        assert_eq!(unlinked.nlink(), 1);
        assert_ne!(
            (unlinked.ctime(), unlinked.ctime_nsec()),
            (published.ctime(), published.ctime_nsec())
        );
        assert_eq!(
            (unlinked.mtime(), unlinked.mtime_nsec()),
            (published.mtime(), published.mtime_nsec())
        );
        snapshot.validate_memory_generation(1024).unwrap();
        let mut memory = snapshot.duplicate_memory_file_for_mapping(1024).unwrap();
        let mut bytes = Vec::new();
        memory.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vec![0x5a_u8; 1024]);
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
