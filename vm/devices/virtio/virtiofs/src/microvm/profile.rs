// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The fixed virtio-fs contract used by the microVM profile.

use super::policy::SubtreePolicy;
#[cfg(windows)]
use anyhow::Context as _;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// Stable device-private attachment identifier of the microVM share.
pub const MICROVM_ATTACHMENT_ID: &str = "fs:microvm0";

/// The mount tag of the microVM share.
pub const MICROVM_MOUNT_TAG: &str = "microvm";

/// The largest number of host directories that a microVM aggregate exposes.
pub const MICROVM_AGGREGATE_MAX_CHILDREN: usize = 256;

/// The longest name of a child of a microVM aggregate, in bytes. It mirrors
/// `openvmm_defs::microvm::MICROVM_FILESYSTEM_MAX_CHILD_NAME`, so the device
/// enforces the same name rule as the VM configuration.
pub const MICROVM_AGGREGATE_MAX_CHILD_NAME: usize = 64;

/// Returns the mount tag of the fixed microVM slot identified by
/// `attachment_id`, or `None` when it is not the slot's identifier, the only
/// identity that the microVM ABI accepts.
pub fn microvm_mount_tag(attachment_id: &str) -> Option<&'static str> {
    (attachment_id == MICROVM_ATTACHMENT_ID).then_some(MICROVM_MOUNT_TAG)
}

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

/// Host identity that performs the guest's filesystem operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MicroVmOwnerMode {
    /// Run every operation as the VMM process, which then owns the files that
    /// the guest creates.
    Vmm,
    /// Run each operation as the UID and GID of its guest caller, without
    /// supplementary groups or capabilities. Guest UID 0 and GID 0 are replaced
    /// by the owner of the export root, which therefore must not be UID 0 or
    /// GID 0. Requires a Linux host.
    Caller,
}

/// Errors in the fixed microVM virtio-fs attachment contract.
#[derive(Debug, thiserror::Error)]
pub enum MicroVmProfileError {
    /// The resource did not identify a fixed microVM filesystem slot.
    #[error("microVM virtio-fs stable ID is not a fixed slot")]
    InvalidStableId,
    /// The identity did not use the documented bounded attachment format.
    #[error("microVM virtio-fs root identity is empty or exceeds {MAX_ROOT_IDENTITY_SIZE} bytes")]
    InvalidRootIdentity,
    /// The supplied host attachment does not match the resource identity.
    #[error("microVM virtio-fs host root identity does not match the attachment")]
    RootIdentityMismatch,
    /// The denied-path list was not canonical.
    #[error("microVM virtio-fs denied paths are invalid")]
    InvalidDeniedPaths,
    /// The allowed-path list was not canonical, or an allowed path was not
    /// inside a denied path.
    #[error("microVM virtio-fs allowed paths are invalid")]
    InvalidAllowedPaths,
    /// The writable-path list was not canonical, overlapped, was hidden, or
    /// belonged to a read-only share.
    #[error("microVM virtio-fs writable paths are invalid")]
    InvalidWritablePaths,
    /// Caller ownership needs per-thread filesystem credentials, which only
    /// Linux provides.
    #[error("microVM virtio-fs caller ownership requires a Linux host")]
    UnsupportedOwnerMode,
    /// The aggregate had no children, too many children, or a child whose
    /// name was invalid or not unique.
    #[error(
        "microVM virtio-fs aggregate requires 1 to {MICROVM_AGGREGATE_MAX_CHILDREN} children with unique, valid names"
    )]
    InvalidAggregateChildren,
}

/// One host directory that a microVM aggregate exposes as a named child of
/// its synthetic root, with its own access policy.
///
/// Like [`MicroVmVirtioFsProfile`], it contains no host path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MicroVmAggregateChild {
    name: String,
    root_identity: Vec<u8>,
    access_mode: MicroVmAccessMode,
    policy: SubtreePolicy,
}

impl MicroVmAggregateChild {
    /// Builds a child from the fields of its resource: its name under the
    /// synthetic root, the identity of its host root, and its access policy,
    /// which [`MicroVmVirtioFsProfile::from_attachment_with_policy`] describes.
    ///
    /// The name must be 1 to [`MICROVM_AGGREGATE_MAX_CHILD_NAME`] ASCII
    /// letters, digits, `.`, `_`, or `-`, other than `.` and `..`.
    pub fn new(
        name: String,
        root_identity: Vec<u8>,
        read_only: bool,
        denied_paths: Vec<String>,
        allowed_paths: Vec<String>,
        writable_paths: Vec<String>,
    ) -> Result<Self, MicroVmProfileError> {
        if name.is_empty()
            || name.len() > MICROVM_AGGREGATE_MAX_CHILD_NAME
            || matches!(name.as_str(), "." | "..")
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(MicroVmProfileError::InvalidAggregateChildren);
        }
        validate_root_identity_size(&root_identity)?;
        Ok(Self {
            name,
            root_identity,
            access_mode: access_mode(read_only),
            policy: subtree_policy(denied_paths, allowed_paths, writable_paths, read_only)?,
        })
    }

    /// Returns the child's name under the synthetic root.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the opaque, platform-specific identity of the child's host
    /// root.
    pub fn root_identity(&self) -> &[u8] {
        &self.root_identity
    }

    /// Returns the host-enforced access policy of the child.
    pub const fn access_mode(&self) -> MicroVmAccessMode {
        self.access_mode
    }

    /// Returns whether host mutations of the child must be rejected.
    pub const fn is_readonly(&self) -> bool {
        matches!(self.access_mode, MicroVmAccessMode::ReadOnly)
    }

    /// Returns the child's access policy.
    pub(crate) fn subtree_policy(&self) -> &SubtreePolicy {
        &self.policy
    }

    /// Validates that `root_path` is the host root identified by this child.
    pub fn validate_root_path(
        &self,
        root_path: impl AsRef<Path>,
    ) -> Result<(), MicroVmProfileError> {
        validate_root_path(&self.root_identity, root_path)
    }

    /// Validates the child's root reached through an already-opened volume and
    /// confirms that `root_path` still names it.
    pub(crate) fn validate_opened_root(
        &self,
        root_path: impl AsRef<Path>,
        stat: &lx::Stat,
    ) -> Result<(), MicroVmProfileError> {
        validate_opened_root(&self.root_identity, root_path, stat)
    }
}

fn access_mode(read_only: bool) -> MicroVmAccessMode {
    if read_only {
        MicroVmAccessMode::ReadOnly
    } else {
        MicroVmAccessMode::ReadWrite
    }
}

fn validate_root_identity_size(root_identity: &[u8]) -> Result<(), MicroVmProfileError> {
    if root_identity.is_empty() || root_identity.len() > MAX_ROOT_IDENTITY_SIZE {
        return Err(MicroVmProfileError::InvalidRootIdentity);
    }
    Ok(())
}

fn subtree_policy(
    denied_paths: Vec<String>,
    allowed_paths: Vec<String>,
    writable_paths: Vec<String>,
    read_only: bool,
) -> Result<SubtreePolicy, MicroVmProfileError> {
    SubtreePolicy::new(
        parse_policy_paths(denied_paths, MicroVmProfileError::InvalidDeniedPaths, true)?,
        parse_policy_paths(
            allowed_paths,
            MicroVmProfileError::InvalidAllowedPaths,
            false,
        )?,
        parse_policy_paths(
            writable_paths,
            MicroVmProfileError::InvalidWritablePaths,
            false,
        )?,
        read_only,
    )
}

/// Parses unique, canonical share-relative paths in lexical order, which is
/// the order that the snapshot contract records. With `allow_root`, the empty
/// path names the share's root.
fn parse_policy_paths(
    paths: Vec<String>,
    error: MicroVmProfileError,
    allow_root: bool,
) -> Result<Vec<PathBuf>, MicroVmProfileError> {
    if paths.len() > 128 || paths.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(error);
    }
    let mut parsed = Vec::with_capacity(paths.len());
    for path in paths {
        if path.is_empty() && allow_root {
            parsed.push(PathBuf::new());
            continue;
        }
        if path.is_empty()
            || path.len() > 4096
            || path.starts_with('/')
            || path.ends_with('/')
            || path.chars().any(|character| {
                character.is_whitespace() || matches!(character, '\0' | '\\' | ':')
            })
        {
            return Err(error);
        }
        let mut relative = PathBuf::new();
        for component in path.split('/') {
            if component.is_empty() || matches!(component, "." | "..") {
                return Err(error);
            }
            relative.push(component);
        }
        parsed.push(relative);
    }
    Ok(parsed)
}

/// Validates that `root_path` is the host root identified by `root_identity`.
fn validate_root_path(
    root_identity: &[u8],
    root_path: impl AsRef<Path>,
) -> Result<(), MicroVmProfileError> {
    match microvm_root_identity(root_path) {
        Ok(identity) if identity == root_identity => Ok(()),
        Ok(_) | Err(_) => Err(MicroVmProfileError::RootIdentityMismatch),
    }
}

/// Validates the root object reached through an already-opened volume against
/// `root_identity`, and confirms that the attachment path still names it.
fn validate_opened_root(
    root_identity: &[u8],
    root_path: impl AsRef<Path>,
    stat: &lx::Stat,
) -> Result<(), MicroVmProfileError> {
    #[cfg(unix)]
    {
        let mut identity = b"openvmm-microvm-fs-unix-v1\0".to_vec();
        identity.extend_from_slice(&stat.device_nr.to_le_bytes());
        identity.extend_from_slice(&stat.inode_nr.to_le_bytes());
        if identity != root_identity {
            return Err(MicroVmProfileError::RootIdentityMismatch);
        }
    }

    #[cfg(windows)]
    {
        const PREFIX: &[u8] = b"openvmm-microvm-fs-windows-v1\0";
        let identity = root_identity
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

    validate_root_path(root_identity, root_path)
}

/// Immutable profile settings for the microVM virtio-fs device.
///
/// The profile intentionally contains no host path. The host path is a
/// process-local attachment supplied when constructing or restoring the
/// filesystem and is never part of device-private saved state.
///
/// A profile describes either a single share or an aggregate, whose synthetic
/// read-only root lists one named child per host directory. Each child carries
/// its own root identity and access policy, so an aggregate has neither.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MicroVmVirtioFsProfile {
    stable_id: String,
    mount_tag: &'static str,
    root_identity: Vec<u8>,
    access_mode: MicroVmAccessMode,
    policy: SubtreePolicy,
    owner_mode: MicroVmOwnerMode,
    children: Vec<MicroVmAggregateChild>,
}

impl MicroVmVirtioFsProfile {
    /// Builds the constrained microVM profile from the resource attachment
    /// fields. `stable_id`, `root_identity`, and `read_only` correspond
    /// exactly to `VirtioFsProfile::Microvm`; `stable_id` selects the fixed
    /// slot and therefore the mount tag. The guest's operations run as the
    /// VMM until [`Self::with_owner_mode`] selects otherwise.
    pub fn from_attachment(
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        denied_paths: Vec<String>,
    ) -> Result<Self, MicroVmProfileError> {
        Self::from_attachment_with_policy(
            stable_id,
            root_identity,
            read_only,
            denied_paths,
            Vec::new(),
            Vec::new(),
        )
    }

    /// Builds the profile like [`Self::from_attachment`], with the complete
    /// access policy of the share: `allowed_paths` exposes subtrees of
    /// `denied_paths` again, and `writable_paths`, when not empty, are the only
    /// subtrees of a read-write share that the guest can modify. Each list
    /// holds unique, canonical share-relative paths in lexical order.
    pub fn from_attachment_with_policy(
        stable_id: String,
        root_identity: Vec<u8>,
        read_only: bool,
        denied_paths: Vec<String>,
        allowed_paths: Vec<String>,
        writable_paths: Vec<String>,
    ) -> Result<Self, MicroVmProfileError> {
        let mount_tag =
            microvm_mount_tag(&stable_id).ok_or(MicroVmProfileError::InvalidStableId)?;
        validate_root_identity_size(&root_identity)?;
        let policy = subtree_policy(denied_paths, allowed_paths, writable_paths, read_only)?;
        Ok(Self {
            stable_id,
            mount_tag,
            root_identity,
            access_mode: access_mode(read_only),
            policy,
            owner_mode: MicroVmOwnerMode::Vmm,
            children: Vec::new(),
        })
    }

    /// Builds the profile of an aggregate from the fields of
    /// `VirtioFsProfile::MicrovmAggregate`. The children keep their order,
    /// which the guest sees in the synthetic root and saved state records.
    /// The aggregate is read-write when any child is.
    pub fn from_aggregate(
        stable_id: String,
        children: Vec<MicroVmAggregateChild>,
    ) -> Result<Self, MicroVmProfileError> {
        let mount_tag =
            microvm_mount_tag(&stable_id).ok_or(MicroVmProfileError::InvalidStableId)?;
        if children.is_empty()
            || children.len() > MICROVM_AGGREGATE_MAX_CHILDREN
            || children.iter().enumerate().any(|(index, child)| {
                children[..index]
                    .iter()
                    .any(|other| other.name == child.name)
            })
        {
            return Err(MicroVmProfileError::InvalidAggregateChildren);
        }
        Ok(Self {
            stable_id,
            mount_tag,
            root_identity: Vec::new(),
            access_mode: access_mode(children.iter().all(MicroVmAggregateChild::is_readonly)),
            policy: SubtreePolicy::default(),
            owner_mode: MicroVmOwnerMode::Vmm,
            children,
        })
    }

    /// Selects the host identity that performs the guest's filesystem
    /// operations.
    pub fn with_owner_mode(
        mut self,
        owner_mode: MicroVmOwnerMode,
    ) -> Result<Self, MicroVmProfileError> {
        if owner_mode == MicroVmOwnerMode::Caller && !cfg!(target_os = "linux") {
            return Err(MicroVmProfileError::UnsupportedOwnerMode);
        }
        self.owner_mode = owner_mode;
        Ok(self)
    }

    /// Returns the stable attachment ID used to match a restore attachment.
    pub fn attachment_id(&self) -> &str {
        &self.stable_id
    }

    /// Returns the opaque, platform-specific root identity from the resource
    /// attachment, which is empty for an aggregate.
    pub fn root_identity(&self) -> &[u8] {
        &self.root_identity
    }

    /// Returns whether the profile describes an aggregate.
    pub fn is_aggregate(&self) -> bool {
        !self.children.is_empty()
    }

    /// Returns the children of an aggregate in order, or nothing for a single
    /// share.
    pub fn children(&self) -> &[MicroVmAggregateChild] {
        &self.children
    }

    /// Validates that `root_path` is the host attachment identified by this
    /// profile.
    pub fn validate_root_path(
        &self,
        root_path: impl AsRef<Path>,
    ) -> Result<(), MicroVmProfileError> {
        validate_root_path(&self.root_identity, root_path)
    }

    /// Validates the root object reached through an already-opened volume and
    /// confirms that the attachment path still names the configured root.
    pub(crate) fn validate_opened_root(
        &self,
        root_path: impl AsRef<Path>,
        stat: &lx::Stat,
    ) -> Result<(), MicroVmProfileError> {
        validate_opened_root(&self.root_identity, root_path, stat)
    }

    /// Returns the fixed FUSE mount tag of the profile's slot.
    pub const fn mount_tag(&self) -> &'static str {
        self.mount_tag
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

    /// Returns the host identity that performs the guest's operations.
    pub const fn owner_mode(&self) -> MicroVmOwnerMode {
        self.owner_mode
    }

    /// Returns canonical host-relative paths hidden from the guest.
    pub fn denied_paths(&self) -> &[PathBuf] {
        self.policy.denied_paths()
    }

    /// Returns canonical host-relative paths that are exposed again inside
    /// denied paths.
    pub fn allowed_paths(&self) -> &[PathBuf] {
        self.policy.allowed_paths()
    }

    /// Returns the canonical host-relative paths that are the only writable
    /// parts of a read-write share, or nothing when the whole share is
    /// writable.
    pub fn writable_paths(&self) -> &[PathBuf] {
        self.policy.writable_paths()
    }

    /// Returns the share's access policy.
    pub(crate) fn subtree_policy(&self) -> &SubtreePolicy {
        &self.policy
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
            vec!["secrets".to_owned()],
        )
        .unwrap();
        assert_eq!(profile.attachment_id(), "fs:microvm0");
        assert_eq!(profile.mount_tag(), "microvm");
        assert_eq!(profile.request_queues(), 1);
        assert!(profile.is_readonly());
        assert_eq!(profile.denied_paths(), &[PathBuf::from("secrets")]);
        assert_eq!(profile.entry_cache_timeout(), Duration::ZERO);
        assert_eq!(profile.attribute_cache_timeout(), Duration::ZERO);
        assert!(profile.direct_io());
        assert!(!profile.has_shared_memory());
        assert!(!profile.packed_rings());
        assert_eq!(profile.fuse_negotiation().protocol_major(), 7);
        assert_eq!(profile.fuse_negotiation().minimum_minor(), 31);
        assert_eq!(profile.fuse_negotiation().maximum_write(), 1024 * 1024);
        assert_eq!(profile.owner_mode(), MicroVmOwnerMode::Vmm);
        profile
            .validate_root_path(temporary_directory.path())
            .unwrap();
    }

    #[test]
    fn profile_accepts_only_the_fixed_slot() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let root_identity = microvm_root_identity(temporary_directory.path()).unwrap();
        assert_eq!(
            microvm_mount_tag(MICROVM_ATTACHMENT_ID),
            Some(MICROVM_MOUNT_TAG)
        );
        for stable_id in ["fs:microvm1", "fs:microvm2", "microvm", ""] {
            assert_eq!(microvm_mount_tag(stable_id), None);
            assert!(matches!(
                MicroVmVirtioFsProfile::from_attachment(
                    stable_id.to_owned(),
                    root_identity.clone(),
                    false,
                    Vec::new(),
                ),
                Err(MicroVmProfileError::InvalidStableId)
            ));
        }
    }

    fn child(name: &str, read_only: bool) -> MicroVmAggregateChild {
        MicroVmAggregateChild::new(
            name.to_owned(),
            b"identity".to_vec(),
            read_only,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn aggregate_profile_is_read_write_when_any_child_is() {
        let read_only = MicroVmVirtioFsProfile::from_aggregate(
            MICROVM_ATTACHMENT_ID.to_owned(),
            vec![child("a", true), child("b", true)],
        )
        .unwrap();
        assert!(read_only.is_aggregate());
        assert!(read_only.is_readonly());
        assert!(read_only.root_identity().is_empty());
        assert_eq!(read_only.mount_tag(), MICROVM_MOUNT_TAG);
        assert_eq!(
            read_only
                .children()
                .iter()
                .map(MicroVmAggregateChild::name)
                .collect::<Vec<_>>(),
            ["a", "b"]
        );

        let read_write = MicroVmVirtioFsProfile::from_aggregate(
            MICROVM_ATTACHMENT_ID.to_owned(),
            vec![child("a", true), child("b", false)],
        )
        .unwrap();
        assert_eq!(read_write.access_mode(), MicroVmAccessMode::ReadWrite);
        assert!(read_write.children()[0].is_readonly());
        assert!(!read_write.children()[1].is_readonly());
    }

    #[test]
    fn aggregate_profile_rejects_invalid_children() {
        let aggregate = |children| {
            MicroVmVirtioFsProfile::from_aggregate(MICROVM_ATTACHMENT_ID.to_owned(), children)
        };
        for children in [
            Vec::new(),
            vec![child("a", true), child("a", false)],
            (0..=MICROVM_AGGREGATE_MAX_CHILDREN)
                .map(|index| child(&index.to_string(), true))
                .collect(),
        ] {
            assert!(matches!(
                aggregate(children),
                Err(MicroVmProfileError::InvalidAggregateChildren)
            ));
        }
        assert!(matches!(
            MicroVmVirtioFsProfile::from_aggregate(
                "fs:microvm1".to_owned(),
                vec![child("a", true)]
            ),
            Err(MicroVmProfileError::InvalidStableId)
        ));
        for name in ["", ".", "..", "a/b", "a\0b", "a\\b", "a:b"] {
            assert!(matches!(
                MicroVmAggregateChild::new(
                    name.to_owned(),
                    b"identity".to_vec(),
                    true,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                ),
                Err(MicroVmProfileError::InvalidAggregateChildren)
            ));
        }
        assert!(matches!(
            MicroVmAggregateChild::new(
                "a".to_owned(),
                Vec::new(),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new()
            ),
            Err(MicroVmProfileError::InvalidRootIdentity)
        ));
        assert!(matches!(
            MicroVmAggregateChild::new(
                "a".to_owned(),
                b"identity".to_vec(),
                true,
                Vec::new(),
                Vec::new(),
                vec!["out".to_owned()],
            ),
            Err(MicroVmProfileError::InvalidWritablePaths)
        ));
    }

    #[test]
    fn aggregate_child_names_follow_the_vm_configuration_rule() {
        let named = |name: &str| {
            MicroVmAggregateChild::new(
                name.to_owned(),
                b"identity".to_vec(),
                true,
                Vec::new(),
                Vec::new(),
                Vec::new(),
            )
        };
        let longest = "a".repeat(MICROVM_AGGREGATE_MAX_CHILD_NAME);
        for name in [longest.as_str(), "0", "a.b_c-D9", "...", ".hidden"] {
            assert_eq!(named(name).unwrap().name(), name);
        }
        let too_long = "a".repeat(MICROVM_AGGREGATE_MAX_CHILD_NAME + 1);
        for name in [too_long.as_str(), "caf\u{e9}", "a b", "a,b", ".", ".."] {
            assert!(matches!(
                named(name),
                Err(MicroVmProfileError::InvalidAggregateChildren)
            ));
        }
    }

    #[test]
    fn profile_selects_caller_ownership_only_on_linux() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            microvm_root_identity(temporary_directory.path()).unwrap(),
            false,
            Vec::new(),
        )
        .unwrap();
        let vmm = profile.clone().with_owner_mode(MicroVmOwnerMode::Vmm);
        assert_eq!(vmm.unwrap(), profile);

        let caller = profile.clone().with_owner_mode(MicroVmOwnerMode::Caller);
        if cfg!(target_os = "linux") {
            let caller = caller.unwrap();
            assert_eq!(caller.owner_mode(), MicroVmOwnerMode::Caller);
            assert_ne!(caller, profile);
        } else {
            assert!(matches!(
                caller,
                Err(MicroVmProfileError::UnsupportedOwnerMode)
            ));
        }
    }

    #[test]
    fn profile_rejects_an_attachment_identity_mismatch() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let profile = MicroVmVirtioFsProfile::from_attachment(
            MICROVM_ATTACHMENT_ID.to_owned(),
            b"not-a-root-identity".to_vec(),
            false,
            Vec::new(),
        )
        .unwrap();

        assert!(
            profile
                .validate_root_path(temporary_directory.path())
                .is_err()
        );
    }

    #[test]
    fn profile_rejects_noncanonical_or_overlapping_denied_paths() {
        let root = tempfile::tempdir().unwrap();
        let identity = microvm_root_identity(root.path()).unwrap();
        for denied_paths in [
            vec!["nested/path".to_owned(), "alpha".to_owned()],
            vec!["secrets".to_owned(), "secrets/nested".to_owned()],
            vec!["../outside".to_owned()],
            vec!["alternate:name".to_owned()],
        ] {
            assert!(
                MicroVmVirtioFsProfile::from_attachment(
                    MICROVM_ATTACHMENT_ID.to_owned(),
                    identity.clone(),
                    true,
                    denied_paths,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn profile_carries_a_canonical_subtree_policy() {
        let root = tempfile::tempdir().unwrap();
        let identity = microvm_root_identity(root.path()).unwrap();
        fn owned(paths: &[&str]) -> Vec<String> {
            paths.iter().map(|path| (*path).to_owned()).collect()
        }
        let with_policy =
            |read_only: bool, denied: &[&str], allowed: &[&str], writable: &[&str]| {
                MicroVmVirtioFsProfile::from_attachment_with_policy(
                    MICROVM_ATTACHMENT_ID.to_owned(),
                    identity.clone(),
                    read_only,
                    owned(denied),
                    owned(allowed),
                    owned(writable),
                )
            };

        // Paths are in the lexical order of the snapshot contract, in which a
        // sibling such as `logs-old` sorts before `logs/...`.
        let profile = with_policy(
            false,
            &["logs", "logs-old"],
            &["logs/payloads"],
            &["out", "out-cache"],
        )
        .unwrap();
        assert_eq!(
            profile.denied_paths(),
            [PathBuf::from("logs"), PathBuf::from("logs-old")]
        );
        assert_eq!(
            profile.allowed_paths(),
            [["logs", "payloads"].iter().collect::<PathBuf>()]
        );
        assert_eq!(
            profile.writable_paths(),
            [PathBuf::from("out"), PathBuf::from("out-cache")]
        );
        assert!(profile.subtree_policy().restricts_writes());

        for (read_only, denied, allowed, writable) in [
            (false, vec!["logs"], vec!["payloads"], vec![]),
            (false, vec!["logs"], vec!["logs/b", "logs/a"], vec![]),
            (false, vec!["logs"], vec!["logs/../escape"], vec![]),
            (true, vec![], vec![], vec!["out"]),
            (false, vec![], vec![], vec!["out", "out"]),
            (false, vec![], vec![], vec!["out/"]),
            (false, vec!["logs"], vec![], vec!["logs/out"]),
        ] {
            assert!(with_policy(read_only, &denied, &allowed, &writable).is_err());
        }
    }
}
