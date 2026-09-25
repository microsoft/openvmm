// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The fixed virtio-fs contract used by the microVM profile.

#[cfg(windows)]
use anyhow::Context as _;
use std::path::Path;
use std::time::Duration;

/// Stable device-private attachment identifier for the microVM share.
pub const MICROVM_ATTACHMENT_ID: &str = "fs:microvm0";

/// The only mount tag accepted by the microVM ABI.
pub const MICROVM_MOUNT_TAG: &str = "microvm";

/// The number of FUSE request queues in the microVM ABI.
pub const MICROVM_REQUEST_QUEUES: u32 = 1;

/// The minimum FUSE minor version with which the microVM protocol contract is
/// compatible.
pub const MICROVM_FUSE_MIN_MINOR: u32 = 31;

/// The FUSE major version used by the microVM protocol contract.
pub const MICROVM_FUSE_MAJOR: u32 = 7;

/// The largest FUSE request payload negotiated by the microVM profile.
pub const MICROVM_FUSE_MAX_WRITE: u32 = 1024 * 1024;

const MAX_ROOT_IDENTITY_SIZE: usize = 4096;

/// Fixed portions of the FUSE negotiation contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MicroVmFuseNegotiationPolicy;

impl MicroVmFuseNegotiationPolicy {
    /// Returns the required FUSE major protocol version.
    pub const fn protocol_major(self) -> u32 {
        MICROVM_FUSE_MAJOR
    }

    /// Returns the oldest compatible FUSE minor protocol version.
    pub const fn minimum_minor(self) -> u32 {
        MICROVM_FUSE_MIN_MINOR
    }

    /// Returns the exact maximum FUSE write size.
    pub const fn maximum_write(self) -> u32 {
        MICROVM_FUSE_MAX_WRITE
    }

    /// Returns whether `READDIRPLUS` and `READDIRPLUS_AUTO` are requested
    /// whenever the guest advertises them.
    pub const fn requests_readdirplus(self) -> bool {
        true
    }

    /// Returns whether direct-I/O shared mappings are requested whenever the
    /// guest advertises the extended flag.
    pub const fn requests_direct_io_allow_mmap(self) -> bool {
        true
    }
}

/// Access policy enforced by the host filesystem implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicroVmAccessMode {
    /// Reject mutations before they reach the host filesystem.
    ReadOnly,
    /// Permit the common, host-supported mutation set.
    ReadWrite,
}

/// Errors in the fixed microVM virtio-fs attachment contract.
#[derive(Debug, thiserror::Error)]
pub enum MicroVmProfileError {
    /// The resource did not identify the only microVM filesystem attachment.
    #[error("microVM virtio-fs stable ID must be {MICROVM_ATTACHMENT_ID}")]
    InvalidStableId,
    /// The identity did not use the documented bounded attachment format.
    #[error("microVM virtio-fs root identity is empty or exceeds {MAX_ROOT_IDENTITY_SIZE} bytes")]
    InvalidRootIdentity,
    /// The supplied host attachment does not match the resource identity.
    #[error("microVM virtio-fs host root identity does not match the attachment")]
    RootIdentityMismatch,
}

/// Immutable profile settings for the microVM virtio-fs device.
///
/// The profile intentionally contains no host path. The host path is a
/// process-local attachment supplied when constructing or restoring the
/// filesystem and is never part of device-private saved state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MicroVmVirtioFsProfile {
    stable_id: String,
    root_identity: Vec<u8>,
    access_mode: MicroVmAccessMode,
}

impl MicroVmVirtioFsProfile {
    /// Builds the constrained microVM profile from the resource attachment
    /// fields. `stable_id`, `root_identity`, and `read_only` correspond
    /// exactly to `VirtioFsProfile::Microvm`.
    pub fn from_attachment(
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
    ) -> Result<Self, MicroVmProfileError> {
        if stable_id != MICROVM_ATTACHMENT_ID {
            return Err(MicroVmProfileError::InvalidStableId);
        }
        if root_identity.is_empty() || root_identity.len() > MAX_ROOT_IDENTITY_SIZE {
            return Err(MicroVmProfileError::InvalidRootIdentity);
        }
        Ok(Self {
            stable_id,
            root_identity,
            access_mode: if read_only {
                MicroVmAccessMode::ReadOnly
            } else {
                MicroVmAccessMode::ReadWrite
            },
        })
    }

    /// Returns the stable attachment ID used to match a restore attachment.
    pub fn attachment_id(&self) -> &str {
        &self.stable_id
    }

    /// Returns the opaque, platform-specific root identity from the resource
    /// attachment.
    pub fn root_identity(&self) -> &[u8] {
        &self.root_identity
    }

    /// Validates that `root_path` is the host attachment identified by this
    /// profile.
    pub fn validate_root_path(
        &self,
        root_path: impl AsRef<Path>,
    ) -> Result<(), MicroVmProfileError> {
        match microvm_root_identity(root_path) {
            Ok(identity) if identity == self.root_identity => Ok(()),
            Ok(_) | Err(_) => Err(MicroVmProfileError::RootIdentityMismatch),
        }
    }

    /// Validates the root object reached through an already-opened volume and
    /// confirms that the attachment path still names the configured root.
    pub(crate) fn validate_opened_root(
        &self,
        root_path: impl AsRef<Path>,
        stat: &lx::Stat,
    ) -> Result<(), MicroVmProfileError> {
        #[cfg(unix)]
        {
            let mut identity = b"openvmm-microvm-fs-unix-v1\0".to_vec();
            identity.extend_from_slice(&stat.device_nr.to_le_bytes());
            identity.extend_from_slice(&stat.inode_nr.to_le_bytes());
            if identity != self.root_identity {
                return Err(MicroVmProfileError::RootIdentityMismatch);
            }
        }

        #[cfg(windows)]
        {
            const PREFIX: &[u8] = b"openvmm-microvm-fs-windows-v1\0";
            let identity = self
                .root_identity
                .strip_prefix(PREFIX)
                .ok_or(MicroVmProfileError::RootIdentityMismatch)?;
            let length = identity
                .get(..size_of::<u32>())
                .and_then(|bytes| bytes.try_into().ok())
                .map(u32::from_le_bytes)
                .ok_or(MicroVmProfileError::RootIdentityMismatch)?;
            let volume_bytes = usize::try_from(length)
                .ok()
                .and_then(|length| length.checked_mul(size_of::<u16>()))
                .ok_or(MicroVmProfileError::RootIdentityMismatch)?;
            let file_id = identity
                .get(size_of::<u32>() + volume_bytes..)
                .and_then(|bytes| bytes.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or(MicroVmProfileError::RootIdentityMismatch)?;
            if file_id != stat.inode_nr {
                return Err(MicroVmProfileError::RootIdentityMismatch);
            }
        }

        self.validate_root_path(root_path)
    }

    /// Returns the fixed FUSE mount tag.
    pub const fn mount_tag(&self) -> &'static str {
        MICROVM_MOUNT_TAG
    }

    /// Returns the fixed request queue count.
    pub const fn request_queues(&self) -> u32 {
        MICROVM_REQUEST_QUEUES
    }

    /// Returns the host-enforced access policy.
    pub const fn access_mode(&self) -> MicroVmAccessMode {
        self.access_mode
    }

    /// Returns whether host mutations must be rejected.
    pub const fn is_readonly(&self) -> bool {
        matches!(self.access_mode, MicroVmAccessMode::ReadOnly)
    }

    /// Returns the entry-cache lifetime required by the ABI.
    pub const fn entry_cache_timeout(&self) -> Duration {
        Duration::ZERO
    }

    /// Returns the attribute-cache lifetime required by the ABI.
    pub const fn attribute_cache_timeout(&self) -> Duration {
        Duration::ZERO
    }

    /// Returns whether direct I/O is required for every opened file.
    pub const fn direct_io(&self) -> bool {
        true
    }

    /// Returns whether the ABI exposes a DAX/shared-memory region.
    pub const fn has_shared_memory(&self) -> bool {
        false
    }

    /// Returns whether the ABI permits packed virtqueues.
    pub const fn packed_rings(&self) -> bool {
        false
    }

    /// Returns the fixed FUSE negotiation policy.
    pub const fn fuse_negotiation(&self) -> MicroVmFuseNegotiationPolicy {
        MicroVmFuseNegotiationPolicy
    }
}

/// Computes the portable root identity expected in
/// `VirtioFsProfile::Microvm::root_identity`.
pub fn microvm_root_identity(root_path: impl AsRef<Path>) -> anyhow::Result<Vec<u8>> {
    #[expect(
        clippy::disallowed_methods,
        reason = "the attachment identity must resolve the host root's final target"
    )]
    let canonical = std::fs::canonicalize(root_path)?;
    let metadata = std::fs::symlink_metadata(&canonical)?;
    anyhow::ensure!(
        metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
        "microVM filesystem root is not a plain directory"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let mut identity = b"openvmm-microvm-fs-unix-v1\0".to_vec();
        identity.extend_from_slice(&metadata.dev().to_le_bytes());
        identity.extend_from_slice(&metadata.ino().to_le_bytes());
        Ok(identity)
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use std::path::Component;

        let stat = pal::windows::fs::query_stat_lx_by_name(&canonical)
            .context("failed to query the microVM filesystem root identity")?;
        anyhow::ensure!(
            stat.FileId != 0,
            "microVM filesystem root has no stable file identity"
        );
        let volume = canonical
            .components()
            .next()
            .and_then(|component| match component {
                Component::Prefix(prefix) => Some(prefix.as_os_str()),
                _ => None,
            })
            .context("microVM filesystem root has no volume prefix")?;
        let volume = volume.encode_wide().collect::<Vec<_>>();
        let volume_length = u32::try_from(volume.len())
            .context("microVM filesystem volume identity is too long")?;
        let mut identity = b"openvmm-microvm-fs-windows-v1\0".to_vec();
        identity.extend_from_slice(&volume_length.to_le_bytes());
        identity.extend(volume.into_iter().flat_map(u16::to_le_bytes));
        identity.extend_from_slice(&stat.FileId.to_le_bytes());
        Ok(identity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_the_fixed_microvm_abi() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let root_identity = microvm_root_identity(temporary_directory.path()).unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            root_identity,
            true,
        )
        .unwrap();
        assert_eq!(profile.attachment_id(), "fs:microvm0");
        assert_eq!(profile.mount_tag(), "microvm");
        assert_eq!(profile.request_queues(), 1);
        assert!(profile.is_readonly());
        assert_eq!(profile.entry_cache_timeout(), Duration::ZERO);
        assert_eq!(profile.attribute_cache_timeout(), Duration::ZERO);
        assert!(profile.direct_io());
        assert!(!profile.has_shared_memory());
        assert!(!profile.packed_rings());
        assert_eq!(profile.fuse_negotiation().protocol_major(), 7);
        assert_eq!(profile.fuse_negotiation().minimum_minor(), 31);
        assert_eq!(profile.fuse_negotiation().maximum_write(), 1024 * 1024);
        profile
            .validate_root_path(temporary_directory.path())
            .unwrap();
    }

    #[test]
    fn profile_rejects_an_attachment_identity_mismatch() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            b"not-a-root-identity".to_vec(),
            false,
        )
        .unwrap();

        assert!(
            profile
                .validate_root_path(temporary_directory.path())
                .is_err()
        );
    }
}
