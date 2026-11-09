// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Snapshot format identifiers and manifest format validation: format magic
//! and version, saved-state schema, capture tiers, restore policies,
//! configuration sections, artifact names, and size limits.

use super::MANIFEST_VERSION;
use super::SnapshotManifest;
use super::microvm;
use anyhow::Context;
use mesh::payload::Protobuf;
use mesh::payload::Timestamp;
use sha2::Digest;
use virt::time_abi::TimeAbiCode;
use virt::time_abi::TimeAbiError;

/// Magic identifying the OpenVMM snapshot manifest format.
pub const SNAPSHOT_FORMAT_MAGIC: &[u8] = b"OPENVMM_SNAPSHOT_V2\0";
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
            machine_contract: None,
            format_magic: SNAPSHOT_FORMAT_MAGIC.to_vec(),
            saved_state_schema_version: SAVED_STATE_SCHEMA_VERSION,
            saved_state_root_type: SAVED_STATE_ROOT_TYPE.to_owned(),
            snapshot_tier: String::new(),
            restore_policy: String::new(),
            consumed_config_sections: 0,
            snapshot_id: guid::Guid::ZERO,
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

/// The version of a manifest, which every manifest format, upstream's
/// included, keeps in field 1.
#[derive(Protobuf)]
#[mesh(package = "openvmm.snapshot")]
struct ManifestVersion {
    #[mesh(1)]
    version: u32,
}

/// The format magic of a manifest of [`MANIFEST_VERSION`], which it keeps in
/// field 10.
#[derive(Protobuf)]
#[mesh(package = "openvmm.snapshot")]
struct ManifestFormatMagic {
    #[mesh(10)]
    format_magic: Vec<u8>,
}

/// Decodes a manifest of this format, one that [`validate_manifest_header`]
/// accepts.
///
/// Earlier formats give some field numbers other types, so decoding one of
/// their manifests whole fails as if it were malformed. This decodes the
/// version alone, which every format keeps in field 1, then the magic alone,
/// and refuses any other version or magic with `E_SNAPSHOT_VERSION` before it
/// decodes the rest. The magic comes second, since only the formats of this
/// version agree on the type of field 10.
pub(super) fn decode_manifest(bytes: &[u8]) -> anyhow::Result<SnapshotManifest> {
    let ManifestVersion { version } =
        mesh::payload::decode(bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_version(version)?;
    let ManifestFormatMagic { format_magic } =
        mesh::payload::decode(bytes).context("failed to decode snapshot manifest")?;
    validate_format_magic(&format_magic)?;
    let manifest: SnapshotManifest =
        mesh::payload::decode(bytes).context("failed to decode snapshot manifest")?;
    validate_manifest_header(&manifest)?;
    Ok(manifest)
}

/// Refuses any other manifest version than [`MANIFEST_VERSION`] with
/// `E_SNAPSHOT_VERSION`, which asks for a recapture.
fn validate_manifest_version(version: u32) -> anyhow::Result<()> {
    if version != MANIFEST_VERSION {
        return Err(TimeAbiError::new(
            TimeAbiCode::SnapshotVersion,
            format!(
                "snapshot manifest version {version} is not supported (expected {MANIFEST_VERSION}); recapture the snapshot"
            ),
        )
        .into());
    }
    Ok(())
}

/// Refuses any other format magic than [`SNAPSHOT_FORMAT_MAGIC`] with
/// `E_SNAPSHOT_VERSION`, which asks for a recapture.
fn validate_format_magic(format_magic: &[u8]) -> anyhow::Result<()> {
    if format_magic != SNAPSHOT_FORMAT_MAGIC {
        return Err(TimeAbiError::new(
            TimeAbiCode::SnapshotVersion,
            "snapshot format magic is invalid; recapture the snapshot",
        )
        .into());
    }
    Ok(())
}

/// Validates the manifest's format identity: [`MANIFEST_VERSION`], the
/// matching magic, and the saved-state schema. Any other version or magic is
/// `E_SNAPSHOT_VERSION`, which asks for a recapture.
pub(super) fn validate_manifest_header(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    validate_manifest_version(manifest.version)?;
    validate_format_magic(&manifest.format_magic)?;
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

/// Validates the manifest's machine contract and capture tier. A microVM
/// machine contract must carry valid time ABI records; a manifest without a
/// machine contract, a regular VM's, has none.
pub(super) fn validate_manifest_contents(manifest: &SnapshotManifest) -> anyhow::Result<()> {
    // The time records come first, so a clock parameter on a platform
    // snapshot's command line reports `E_CMDLINE_CLOCK_TOKEN`.
    if let Some(contract) = &manifest.machine_contract {
        super::time::validate_time_abi_contract(contract)?;
    }
    microvm::validate_snapshot_tier(manifest)
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_manifest;
    use super::super::validate_manifest;

    #[test]
    fn other_versions_are_rejected_with_e_snapshot_version() {
        for version in [0, 1, 3, u32::MAX] {
            let mut manifest = test_manifest();
            manifest.version = version;
            let error = format!(
                "{:#}",
                validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err()
            );
            assert!(error.contains("[E_SNAPSHOT_VERSION]"), "{error}");
            assert!(error.contains("recapture the snapshot"), "{error}");
        }
    }

    #[test]
    fn validate_manifest_wrong_magic() {
        for magic in [&b"NOT_OPENVMM"[..], b"OPENVMM_SNAPSHOT_V1\0"] {
            let mut manifest = test_manifest();
            manifest.format_magic = magic.to_vec();
            let err = validate_manifest(&manifest, "x86_64", 1024, 2, 4096).unwrap_err();
            assert!(err.to_string().contains("[E_SNAPSHOT_VERSION]"), "{err}");
            assert!(err.to_string().contains("format magic"), "{err}");
        }
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
