// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot manifest types and I/O functions for saving/restoring VM snapshots.

use anyhow::Context;
use mesh::payload::Protobuf;
use mesh::payload::Timestamp;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

/// Current manifest format version. Bump when making incompatible changes.
pub const MANIFEST_VERSION: u32 = 1;

/// Manifest describing a VM snapshot.
#[derive(Clone, Protobuf)]
#[mesh(package = "openvmm.snapshot")]
pub struct SnapshotManifest {
    /// Manifest format version.
    #[mesh(1)]
    pub version: u32,
    /// When the snapshot was created.
    #[mesh(2)]
    pub created_at: Timestamp,
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
    /// Unique snapshot identifier generated at save time. Zero for snapshots
    /// created before snapshot IDs were introduced.
    #[mesh(8)]
    pub snapshot_id: guid::Guid,
}

/// Returns whether `a` and `b` refer to the same underlying file (as opposed
/// to merely having the same path). Hard links to the same file are reported
/// as the same file.
pub fn same_file(a: &Path, b: &Path) -> io::Result<bool> {
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

/// Validate that a snapshot manifest is compatible with the running VM config.
///
/// Checks version, architecture, memory size, VP count, and page size.
/// Returns `Ok(())` if the manifest matches, or an error describing the
/// first mismatch found.
pub fn validate_manifest(
    manifest: &SnapshotManifest,
    expected_arch: &str,
    expected_memory_size: u64,
    expected_vp_count: u32,
    expected_page_size: u32,
) -> anyhow::Result<()> {
    if manifest.version != MANIFEST_VERSION {
        anyhow::bail!(
            "snapshot manifest version {} is not supported (expected {})",
            manifest.version,
            MANIFEST_VERSION,
        );
    }

    if manifest.architecture != expected_arch {
        anyhow::bail!(
            "snapshot architecture '{}' doesn't match expected '{}'",
            manifest.architecture,
            expected_arch,
        );
    }

    if manifest.memory_size_bytes != expected_memory_size {
        anyhow::bail!(
            "snapshot memory size ({} bytes) doesn't match expected ({} bytes)",
            manifest.memory_size_bytes,
            expected_memory_size,
        );
    }

    if manifest.vp_count != expected_vp_count {
        anyhow::bail!(
            "snapshot VP count ({}) doesn't match expected ({})",
            manifest.vp_count,
            expected_vp_count,
        );
    }

    if manifest.page_size != expected_page_size {
        anyhow::bail!(
            "snapshot page size ({}) doesn't match expected ({})",
            manifest.page_size,
            expected_page_size,
        );
    }

    Ok(())
}

/// Optional checks a caller can require a snapshot to pass on restore.
/// Unset fields are not checked.
#[derive(Default)]
pub struct SnapshotExpectations {
    /// The snapshot ID the manifest must contain.
    pub snapshot_id: Option<guid::Guid>,
}

/// Validate that a snapshot manifest matches the caller's expectations.
pub fn validate_expectations(
    manifest: &SnapshotManifest,
    expectations: &SnapshotExpectations,
) -> anyhow::Result<()> {
    if let Some(expected) = expectations.snapshot_id {
        if manifest.snapshot_id != expected {
            anyhow::bail!("snapshot ID doesn't match the expected snapshot ID");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_ID: guid::Guid = guid::guid!("3099921d-f0e6-48aa-9d58-8fd4811ce4d2");
    const OTHER_ID: guid::Guid = guid::guid!("7b2a8c1e-5f4d-4e3a-9b6c-0d1e2f3a4b5c");

    /// Helper: build a test manifest with sensible defaults.
    fn test_manifest() -> SnapshotManifest {
        SnapshotManifest {
            version: MANIFEST_VERSION,
            created_at: Timestamp {
                seconds: 1234567890,
                nanos: 0,
            },
            openvmm_version: "test-0.1.0".to_string(),
            memory_size_bytes: 1024,
            vp_count: 2,
            page_size: 4096,
            architecture: "x86_64".to_string(),
            snapshot_id: TEST_ID,
        }
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");

        // Create a fake memory backing file in the same directory (same fs).
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"FAKEMEM").unwrap();

        let manifest = test_manifest();
        let state = b"saved-state-data";

        write_snapshot(&snap_dir, &manifest, state, &mem_path).unwrap();

        let (read_manifest, read_state) = read_snapshot(&snap_dir).unwrap();
        assert_eq!(read_manifest.version, manifest.version);
        assert_eq!(read_manifest.memory_size_bytes, manifest.memory_size_bytes);
        assert_eq!(read_manifest.vp_count, manifest.vp_count);
        assert_eq!(read_manifest.architecture, manifest.architecture);
        assert_eq!(read_manifest.snapshot_id, manifest.snapshot_id);
        assert_eq!(read_state, state);

        // memory.bin should exist in the snapshot directory.
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn write_snapshot_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("a").join("b").join("c");

        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();

        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();

        assert!(snap_dir.join("manifest.bin").exists());
        assert!(snap_dir.join("state.bin").exists());
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn write_snapshot_same_memory_path() {
        // When the memory backing file IS <snap_dir>/memory.bin, the function
        // should detect the collision and skip the hard-link.
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();

        let mem_path = snap_dir.join("memory.bin");
        std::fs::write(&mem_path, b"SAMEFILE").unwrap();

        // Should succeed without error.
        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();

        // The file content should be unchanged.
        assert_eq!(std::fs::read(&mem_path).unwrap(), b"SAMEFILE");
    }

    #[test]
    fn write_snapshot_rejects_existing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();

        let mut first = test_manifest();
        first.snapshot_id = OTHER_ID;
        write_snapshot(&snap_dir, &first, b"first", &mem_path).unwrap();

        let second = test_manifest();
        let err = write_snapshot(&snap_dir, &second, b"second", &mem_path).unwrap_err();
        assert!(
            err.to_string().contains("already contains a snapshot"),
            "unexpected error: {err}"
        );

        // The existing snapshot is untouched.
        let (manifest, state) = read_snapshot(&snap_dir).unwrap();
        assert_eq!(manifest.snapshot_id, OTHER_ID);
        assert_eq!(state, b"first");
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn write_snapshot_rejects_foreign_memory_bin() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("memory.bin"), b"OTHER").unwrap();

        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(
            err.to_string().contains("not the VM's memory backing file"),
            "unexpected error: {err}"
        );

        // The unrelated memory.bin is neither deleted nor replaced, and nothing
        // else is left behind.
        assert_eq!(
            std::fs::read(snap_dir.join("memory.bin")).unwrap(),
            b"OTHER"
        );
        assert!(!snap_dir.join("state.bin").exists());
        assert!(!snap_dir.join("manifest.bin").exists());
    }

    #[test]
    fn write_snapshot_accepts_hard_linked_memory_bin() {
        // A memory.bin that is a hard link to the backing file has a different
        // path but is the same file, so it must be accepted.
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();

        let mem_path = dir.path().join("backing.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();
        std::fs::hard_link(&mem_path, snap_dir.join("memory.bin")).unwrap();

        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap();
        assert!(snap_dir.join("manifest.bin").exists());
        assert_eq!(std::fs::read(snap_dir.join("memory.bin")).unwrap(), b"MEM");
    }

    #[test]
    fn write_snapshot_overwrites_leftover_state() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("state.bin"), b"stale").unwrap();

        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();

        write_snapshot(&snap_dir, &test_manifest(), b"fresh", &mem_path).unwrap();
        let (_, state) = read_snapshot(&snap_dir).unwrap();
        assert_eq!(state, b"fresh");
    }

    #[test]
    fn write_snapshot_cleans_up_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        // The backing file doesn't exist, so linking memory.bin fails after
        // state.bin has been written.
        let mem_path = dir.path().join("missing.bin");

        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();

        // Everything this call created, including the directory, is removed.
        assert!(!snap_dir.exists());
    }

    #[test]
    fn write_snapshot_cleanup_keeps_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::write(snap_dir.join("unrelated.txt"), b"keep").unwrap();
        let mem_path = dir.path().join("missing.bin");

        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();

        assert!(!snap_dir.join("state.bin").exists());
        assert!(!snap_dir.join("manifest.bin").exists());
        assert_eq!(
            std::fs::read(snap_dir.join("unrelated.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn check_snapshot_destination_rejects_file() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let err = check_snapshot_destination(&not_a_dir, &dir.path().join("mem")).unwrap_err();
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_snapshot_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // No files written — read should fail.
        let result = read_snapshot(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn validate_manifest_ok() {
        let manifest = test_manifest();
        validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap();
    }

    #[test]
    fn validate_manifest_wrong_arch() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "aarch64", 1024, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("architecture"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_memory_size() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 9999, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("memory size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_vp_count() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 1024, 99, 4096).unwrap_err();
        assert!(
            err.to_string().contains("VP count"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_page_size() {
        let manifest = test_manifest();
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 65536).unwrap_err();
        assert!(
            err.to_string().contains("page size"),
            "unexpected error: {err}"
        );
    }

    fn expect_id(id: guid::Guid) -> SnapshotExpectations {
        SnapshotExpectations {
            snapshot_id: Some(id),
        }
    }

    #[test]
    fn validate_expectations_snapshot_id_matches() {
        let manifest = test_manifest();
        validate_expectations(&manifest, &expect_id(TEST_ID)).unwrap();
    }

    #[test]
    fn validate_expectations_skipped_when_not_provided() {
        let manifest = test_manifest();
        validate_expectations(&manifest, &SnapshotExpectations::default()).unwrap();
    }

    #[test]
    fn validate_expectations_snapshot_id_mismatch() {
        let manifest = test_manifest();
        let err = validate_expectations(&manifest, &expect_id(OTHER_ID)).unwrap_err();
        assert!(
            err.to_string().contains("snapshot ID"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_expectations_snapshot_id_missing_from_manifest() {
        let mut manifest = test_manifest();
        manifest.snapshot_id = guid::Guid::ZERO;
        let err = validate_expectations(&manifest, &expect_id(TEST_ID)).unwrap_err();
        assert!(
            err.to_string().contains("snapshot ID"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_manifest_wrong_version() {
        let mut manifest = test_manifest();
        manifest.version = 999;
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("version"),
            "unexpected error: {err}"
        );
    }
}
