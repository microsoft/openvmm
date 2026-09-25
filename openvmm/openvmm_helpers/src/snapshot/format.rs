// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot format identifiers and manifest format validation: format magic
//! and supported versions, saved-state schema, capture tiers, restore
//! policies, configuration sections, artifact names, and size limits.

use super::MANIFEST_VERSION;
use super::SnapshotManifest;
use super::microvm;
use mesh::payload::Timestamp;
use sha2::Digest;

/// Magic identifying the OpenVMM snapshot manifest format.
pub const SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V5\0";
const VERSION_4_MANIFEST_VERSION: u32 = 4;
const VERSION_4_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V4\0";
pub(super) const PREVIOUS_MANIFEST_VERSION: u32 = 3;
pub(super) const PREVIOUS_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V3\0";
pub(super) const LEGACY_MANIFEST_VERSION: u32 = 2;
pub(super) const LEGACY_SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V2\0";
/// Saved-state schema version used by the VM worker envelope.
pub const SAVED_STATE_SCHEMA_VERSION: u32 = 1;
/// Protobuf root type stored in `state.bin`.
pub const SAVED_STATE_ROOT_TYPE: &str = "openvmm.SavedState";
/// Fleet-wide snapshot captured before image and sandbox configuration is consumed.
pub const SNAPSHOT_TIER_PLATFORM: &str = "platform";
/// Tenant-scoped snapshot captured at a warm workload handoff point.
pub const SNAPSHOT_TIER_WORKLOAD_START: &str = "workload-start";
/// Single-instance checkpoint captured during a live workload.
pub const SNAPSHOT_TIER_INSTANCE_CHECKPOINT: &str = "instance-checkpoint";
/// Reusable restore policy for snapshots that create independent instances.
pub const SNAPSHOT_RESTORE_POLICY_CLONE: &str = "clone";
/// Single-use restore policy for snapshots that continue one instance.
pub const SNAPSHOT_RESTORE_POLICY_RESUME: &str = "resume";
/// The invariant configuration section was consumed before capture.
pub const SNAPSHOT_CONFIG_INVARIANTS: u32 = 1 << 0;
/// The image-binding configuration section was consumed before capture.
pub const SNAPSHOT_CONFIG_IMAGE_BINDING: u32 = 1 << 1;
/// The per-sandbox configuration section was consumed before capture.
pub const SNAPSHOT_CONFIG_SANDBOX: u32 = 1 << 2;
pub(super) const SNAPSHOT_CONFIG_ALL: u32 =
    SNAPSHOT_CONFIG_INVARIANTS | SNAPSHOT_CONFIG_IMAGE_BINDING | SNAPSHOT_CONFIG_SANDBOX;

pub(super) const MANIFEST_FILE_NAME: &str = "manifest.bin";
pub(super) const STATE_FILE_NAME: &str = "state.bin";
pub(super) const MEMORY_FILE_NAME: &str = "memory.bin";
pub(super) const RESUME_CLAIM_FILE_NAME: &str = "resume.claim";
/// Fixed snapshot-relative name of a paired scratch image.
pub const SCRATCH_FILE_NAME: &str = "scratch.img";
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
            machine_contract: None,
            format_magic: SNAPSHOT_FORMAT_MAGIC.to_vec(),
            saved_state_schema_version: SAVED_STATE_SCHEMA_VERSION,
            saved_state_root_type: SAVED_STATE_ROOT_TYPE.to_owned(),
            snapshot_tier: String::new(),
            restore_policy: String::new(),
            consumed_config_sections: 0,
        }
    }
}

pub(super) fn validate_sha256(digest: &[u8], description: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        digest.len() == SHA256_SIZE,
        "{description} SHA-256 digest has invalid length {}",
        digest.len(),
    );
    Ok(())
}

pub(super) fn verify_digest(
    bytes: &[u8],
    expected: &[u8],
    description: &str,
) -> anyhow::Result<()> {
    let actual: [u8; 32] = sha2::Sha256::digest(bytes).into();
    anyhow::ensure!(
        actual.as_slice() == expected,
        "{description} SHA-256 digest mismatch",
    );
    Ok(())
}

pub(super) fn validate_manifest_header(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    let expected_magic = match manifest.version {
        LEGACY_MANIFEST_VERSION => LEGACY_SNAPSHOT_FORMAT_MAGIC,
        PREVIOUS_MANIFEST_VERSION => PREVIOUS_SNAPSHOT_FORMAT_MAGIC,
        VERSION_4_MANIFEST_VERSION => VERSION_4_SNAPSHOT_FORMAT_MAGIC,
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
    if manifest.version < MANIFEST_VERSION {
        anyhow::ensure!(
            manifest.snapshot_tier.is_empty()
                && manifest.restore_policy.is_empty()
                && manifest.consumed_config_sections == 0,
            "snapshot manifest version {} cannot contain snapshot tier metadata",
            manifest.version,
        );
    }
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
            anyhow::ensure!(
                !microvm::has_sandbox_blocks(manifest),
                "snapshot manifest version {LEGACY_MANIFEST_VERSION} cannot contain microVM sandbox blocks"
            );
        }
        PREVIOUS_MANIFEST_VERSION | VERSION_4_MANIFEST_VERSION | MANIFEST_VERSION => {
            anyhow::ensure!(
                manifest.state_sha256.is_empty() && manifest.memory_sha256.is_empty(),
                "snapshot manifest version {} must not contain legacy artifact digests",
                manifest.version,
            );
            if manifest.version == PREVIOUS_MANIFEST_VERSION {
                anyhow::ensure!(
                    !microvm::has_sandbox_blocks(manifest),
                    "snapshot manifest version {PREVIOUS_MANIFEST_VERSION} cannot contain microVM sandbox blocks"
                );
            }
            if manifest.version == MANIFEST_VERSION {
                microvm::validate_snapshot_tier(manifest)?;
            }
        }
        version => anyhow::bail!(
            "snapshot manifest version {version} is not supported (expected {LEGACY_MANIFEST_VERSION} through {MANIFEST_VERSION})"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::microvm::paired_scratch_manifest;
    use super::super::tests::test_manifest;
    use super::super::validate_manifest;
    use super::*;

    #[test]
    fn version_4_manifest_has_no_snapshot_tier_metadata() {
        let scratch = vec![0x5a; 512];
        let mut manifest = paired_scratch_manifest(&scratch);
        manifest.version = VERSION_4_MANIFEST_VERSION;
        manifest.format_magic = VERSION_4_SNAPSHOT_FORMAT_MAGIC.to_vec();
        manifest.snapshot_tier.clear();
        manifest.restore_policy.clear();
        manifest.consumed_config_sections = 0;
        validate_manifest_header(&manifest).unwrap();
        validate_manifest_version(&manifest).unwrap();

        manifest.snapshot_tier = SNAPSHOT_TIER_WORKLOAD_START.to_owned();
        assert!(validate_manifest_version(&manifest).is_err());
    }

    #[test]
    fn current_manifest_rejects_legacy_payload_digests() {
        let mut manifest = test_manifest();
        manifest.state_sha256 = vec![0; SHA256_SIZE];
        let error = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
        assert!(error.to_string().contains("legacy artifact digests"));
    }

    #[test]
    fn previous_v3_manifest_remains_accepted() {
        let mut manifest = test_manifest();
        manifest.version = PREVIOUS_MANIFEST_VERSION;
        manifest.format_magic = PREVIOUS_SNAPSHOT_FORMAT_MAGIC.to_vec();

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
