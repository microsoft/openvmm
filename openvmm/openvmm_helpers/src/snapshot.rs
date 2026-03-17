// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot manifest types and I/O functions for saving/restoring VM snapshots.

use anyhow::Context;
use mesh::payload::Protobuf;
use mesh::payload::Timestamp;
use std::path::Path;

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

#[cfg(test)]
mod tests {
    use super::*;

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
