// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot format identifiers and manifest format validation: format magic
//! and supported versions, saved-state schema, artifact names, and size limits.

use super::MANIFEST_VERSION;
use super::SnapshotManifest;
use mesh::payload::Timestamp;

/// Magic identifying the OpenVMM snapshot manifest format.
pub const SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V3\0";
pub(super) const LEGACY_MANIFEST_VERSION: u32 = 2;
pub(super) const LEGACY_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V2\0";
/// Saved-state schema version used by the VM worker envelope.
pub const SAVED_STATE_SCHEMA_VERSION: u32 = 1;
/// Protobuf root type stored in `state.bin`.
pub const SAVED_STATE_ROOT_TYPE: &str = "openvmm.SavedState";

pub(super) const MANIFEST_FILE_NAME: &str = "manifest.bin";
pub(super) const STATE_FILE_NAME: &str = "state.bin";
pub(super) const MEMORY_FILE_NAME: &str = "memory.bin";
pub(super) const MAX_MANIFEST_SIZE_BYTES: u64 = 1024 * 1024;
pub(super) const MAX_SAVED_STATE_SIZE_BYTES: u64 = 256 * 1024 * 1024;
pub(super) const SHA256_SIZE: usize = 32;

/// An empty manifest in the current snapshot format.
///
/// Capture code sets the identity fields (version, creation time, OpenVMM
/// version, and VM shape) explicitly and takes the rest from this default:
/// the current format magic, saved-state schema, and root type, with every
/// other field empty.
impl Default for SnapshotManifest {
    fn default() -> Self {
        Self {
            version: MANIFEST_VERSION,
            created_at: Timestamp {
                seconds: 0,
                nanos: 0,
            },
            openvmm_version: String::new(),
            memory_size_bytes: 0,
            vp_count: 0,
            page_size: 0,
            architecture: String::new(),
            state_size_bytes: 0,
            state_sha256: Vec::new(),
            memory_sha256: Vec::new(),
            format_magic: SNAPSHOT_FORMAT_MAGIC.to_vec(),
            saved_state_schema_version: SAVED_STATE_SCHEMA_VERSION,
            saved_state_root_type: SAVED_STATE_ROOT_TYPE.to_owned(),
        }
    }
}

pub(super) fn validate_manifest_header(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let expected_magic = match manifest.version {
        LEGACY_MANIFEST_VERSION => LEGACY_SNAPSHOT_FORMAT_MAGIC,
        MANIFEST_VERSION => SNAPSHOT_FORMAT_MAGIC,
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} through {MANIFEST_VERSION})"
        ),
    };
    anyhow::ensure!(
        manifest.format_magic == expected_magic,
        "snapshot format magic is invalid"
    );
    anyhow::ensure!(
        manifest.saved_state_schema_version == SAVED_STATE_SCHEMA_VERSION,
        "snapshot saved-state schema version {} is unsupported",
        manifest.saved_state_schema_version
    );
    anyhow::ensure!(
        manifest.saved_state_root_type == SAVED_STATE_ROOT_TYPE,
        "snapshot saved-state root type '{}' is unsupported",
        manifest.saved_state_root_type
    );
    Ok(())
}

pub(super) fn validate_manifest_version(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    match manifest.version {
        LEGACY_MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.len() == SHA256_SIZE,
                "legacy state.bin SHA-256 digest has invalid length {}",
                manifest.state_sha256.len(),
            );
            anyhow::ensure!(
                manifest.memory_sha256.len() == SHA256_SIZE,
                "legacy memory.bin SHA-256 digest has invalid length {}",
                manifest.memory_sha256.len(),
            );
        }
        MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.is_empty() && manifest.memory_sha256.is_empty(),
                "snapshot manifest version {} must not contain legacy artifact digests",
                manifest.version,
            );
        }
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} through {MANIFEST_VERSION})"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_manifest;
    use super::super::validate_manifest;
    use super::*;

    #[test]
    fn current_manifest_rejects_legacy_payload_digests() {
        let mut manifest = test_manifest();
        manifest.state_sha256 = vec![0; SHA256_SIZE];
        let error = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(error.to_string().contains("legacy artifact digests"));
    }

    #[test]
    fn legacy_v2_manifest_requires_payload_digests() {
        let mut manifest = test_manifest();
        manifest.version = LEGACY_MANIFEST_VERSION;
        manifest.format_magic = LEGACY_SNAPSHOT_FORMAT_MAGIC.to_vec();
        let error = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("legacy state.bin SHA-256 digest")
        );

        manifest.state_sha256 = vec![0; SHA256_SIZE];
        manifest.memory_sha256 = vec![0; SHA256_SIZE];
        validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap();
    }

    #[test]
    fn validate_manifest_wrong_magic() {
        let mut manifest = test_manifest();
        manifest.format_magic = b"NOT_OPENVMM".to_vec();
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(err.to_string().contains("format magic"));
    }

    #[test]
    fn validate_manifest_wrong_saved_state_schema() {
        let mut manifest = test_manifest();
        manifest.saved_state_schema_version += 1;
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(err.to_string().contains("schema version"));
    }

    #[test]
    fn validate_manifest_wrong_saved_state_root() {
        let mut manifest = test_manifest();
        manifest.saved_state_root_type = "other.SavedState".to_owned();
        let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(err.to_string().contains("root type"));
    }
}
