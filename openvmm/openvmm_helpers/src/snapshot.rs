// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot manifest types and I/O functions for saving/restoring VM snapshots.

use mesh::payload::Protobuf;
use mesh::payload::Timestamp;

pub mod format;
pub mod fs;
pub mod microvm;
pub mod publish;
pub mod restore;
pub mod time;

pub use publish::check_snapshot_destination;
pub use publish::write_snapshot;

/// The manifest format version, the only one this OpenVMM reads or writes.
/// Bump when making incompatible changes.
pub const MANIFEST_VERSION: u32 = 6;

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
    /// Length of `state.bin` in bytes.
    #[mesh(8)]
    pub state_size_bytes: u64,
    // Fields 9 and 10 held the artifact digests of manifest version 2. They
    // are retired, and their numbers are never reused.
    /// Authoritative machine composition for versioned machine profiles.
    #[mesh(11)]
    pub machine_contract: Option<microvm::SnapshotMachineContract>,
    /// Snapshot format magic.
    #[mesh(12)]
    pub format_magic: Vec<u8>,
    /// Version of the serialized VM saved-state schema.
    #[mesh(13)]
    pub saved_state_schema_version: u32,
    /// Fully qualified protobuf root type stored in `state.bin`.
    #[mesh(14)]
    pub saved_state_root_type: String,
    /// Sandbox capture tier. Empty for blockless microVM snapshots.
    #[mesh(15)]
    pub snapshot_tier: String,
    /// `clone` for reusable artifacts or `resume` for single-use artifacts.
    #[mesh(16)]
    pub restore_policy: String,
    /// Bitmask of configuration sections consumed before capture.
    #[mesh(17)]
    pub consumed_config_sections: u32,
    /// Unique snapshot identifier generated at save time. Zero if the manifest
    /// has no snapshot ID.
    #[mesh(18, encoding = "mesh::payload::encoding::ZeroCopyEncoding")]
    pub snapshot_id: guid::Guid,
    // Field 19 is reserved for `compatibility_bundle_digest` and field 20 for
    // `memory_artifacts`.
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
    format::validate_manifest_header(manifest)?;
    format::validate_manifest_contents(manifest)?;

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
#[derive(Default)]
pub struct SnapshotExpectations {
    /// If set, the manifest's snapshot ID must equal this value. If `None`,
    /// the snapshot ID is not checked.
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
    use super::restore::read_snapshot;
    use super::*;

    const TEST_ID: guid::Guid = guid::guid!("3099921d-f0e6-48aa-9d58-8fd4811ce4d2");
    const OTHER_ID: guid::Guid = guid::guid!("7b2a8c1e-5f4d-4e3a-9b6c-0d1e2f3a4b5c");

    /// Helper: build a test manifest with sensible defaults.
    pub(super) fn test_manifest() -> SnapshotManifest {
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
            ..Default::default()
        }
    }

    #[test]
    fn write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");

        // Create a fake memory backing file in the same directory (same fs).
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let manifest = test_manifest();
        let state = b"saved-state-data";

        write_snapshot(&snap_dir, &manifest, state, &mem_path).unwrap();

        let (read_manifest, read_state) = read_snapshot(&snap_dir, 1024).unwrap();
        assert_eq!(read_manifest.version, manifest.version);
        assert_eq!(read_manifest.memory_size_bytes, manifest.memory_size_bytes);
        assert_eq!(read_manifest.vp_count, manifest.vp_count);
        assert_eq!(read_manifest.architecture, manifest.architecture);
        assert_eq!(read_manifest.snapshot_id, manifest.snapshot_id);
        assert_eq!(read_manifest.state_size_bytes, state.len() as u64);
        assert_eq!(read_state, state);

        // memory.bin should exist in the snapshot directory.
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn decode_manifest_without_snapshot_id() {
        // Older manifests have no snapshot ID, and NVX v6 manifests use field 8
        // for an integer. Both decode with a zero ID, so the version check can
        // reject unsupported formats with a clear error.
        #[derive(Protobuf)]
        #[mesh(package = "openvmm.snapshot")]
        struct OldManifest {
            #[mesh(1)]
            version: u32,
            #[mesh(8)]
            state_size_bytes: u64,
        }

        let bytes = mesh::payload::encode(OldManifest {
            version: 6,
            state_size_bytes: 4096,
        });
        let manifest: SnapshotManifest = mesh::payload::decode(&bytes).unwrap();
        assert!(manifest.snapshot_id.is_zero());
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(
            err.to_string().contains("version 6 is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_snapshot_rejects_existing_directory() {
        // An existing destination is rejected even if it is empty or holds the
        // VM's own memory backing file, and nothing is written into it.
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        std::fs::create_dir_all(&snap_dir).unwrap();

        let mem_path = snap_dir.join("memory.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();

        let err = write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err}"
        );
        assert_eq!(std::fs::read(&mem_path).unwrap(), b"MEM");
        assert!(!snap_dir.join("state.bin").exists());
        assert!(!snap_dir.join("manifest.bin").exists());
    }

    #[test]
    fn write_snapshot_rejects_existing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        let mem_path = dir.path().join("memory.bin");
        std::fs::write(&mem_path, vec![0_u8; 1024]).unwrap();

        let mut first = test_manifest();
        first.snapshot_id = OTHER_ID;
        write_snapshot(&snap_dir, &first, b"first", &mem_path).unwrap();

        let second = test_manifest();
        let err = write_snapshot(&snap_dir, &second, b"second", &mem_path).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err}"
        );

        // The existing snapshot is untouched.
        let (manifest, state) = read_snapshot(&snap_dir).unwrap();
        assert_eq!(manifest.snapshot_id, OTHER_ID);
        assert_eq!(state, b"first");
        assert!(snap_dir.join("memory.bin").exists());
    }

    #[test]
    fn write_snapshot_cleans_up_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().join("snap");
        // The backing file is shorter than the manifest's memory size, so
        // copying memory.bin fails after state.bin has been written.
        let mem_path = dir.path().join("short.bin");
        std::fs::write(&mem_path, b"MEM").unwrap();

        write_snapshot(&snap_dir, &test_manifest(), b"state", &mem_path).unwrap_err();

        // Everything this call created, including the directory, is removed.
        assert!(!snap_dir.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn check_snapshot_destination_rejects_file() {
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("file");
        std::fs::write(&not_a_dir, b"x").unwrap();
        let err = check_snapshot_destination(&not_a_dir).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_snapshot_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        // No files written — read should fail.
        let result = read_snapshot(dir.path(), 1024);
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
