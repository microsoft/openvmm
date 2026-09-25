// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM filesystem construction, policy hooks, and attachment restore.

use super::MAX_FUSE_REQUEST_BYTES;
use super::profile::MicroVmVirtioFsProfile;
use super::saved_state::MAX_ALIAS_BYTES;
use super::saved_state::MAX_ALIASES;
use super::saved_state::MAX_ALIASES_PER_INODE;
use super::saved_state::MAX_DIRECTORY_BYTES;
use super::saved_state::MAX_DIRECTORY_ENTRIES;
use super::saved_state::MAX_HANDLES;
use super::saved_state::MAX_INODES;
use super::saved_state::SCHEMA_VERSION;
use super::saved_state::SavedHandle;
use super::saved_state::SavedInode;
use super::saved_state::SavedState;
use super::state::encode_relative_path;
use super::state::fuse_negotiation_from_session;
use super::state::reopen_flags;
use super::state::saved_identity;
use super::state::saved_negotiation;
use super::state::session_state_from_saved;
use super::state::validate_identity;
use super::state::validate_microvm_state;
use super::state::validate_relative_path;
use super::state::validate_reopenable_alias;
use super::state::validate_saved_identity;
use crate::ATTRIBUTE_TIMEOUT;
use crate::ENTRY_TIMEOUT;
use crate::FuseNegotiation;
use crate::HandleMap;
use crate::InodeMap;
use crate::VirtioFs;
use crate::VirtioFsInner;
use crate::VirtioFsMode;
use crate::file::VirtioFsFile;
use crate::inode::VirtioFsInode;
use crate::inode::VirtioFsVolume;
use anyhow::Context;
use fuse::Session;
use fuse::SessionInfo;
use fuse::SessionState;
use fuse::protocol::FOPEN_DIRECT_IO;
use fuse::protocol::FUSE_ROOT_ID;
use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn attribute_timeout(fs: &VirtioFs) -> Duration {
    fs.microvm_profile().map_or(
        ATTRIBUTE_TIMEOUT,
        MicroVmVirtioFsProfile::attribute_cache_timeout,
    )
}

pub(crate) fn entry_timeout(fs: &VirtioFs) -> Duration {
    fs.microvm_profile()
        .map_or(ENTRY_TIMEOUT, MicroVmVirtioFsProfile::entry_cache_timeout)
}

pub(crate) fn open_flags(fs: &VirtioFs) -> u32 {
    if fs
        .microvm_profile()
        .is_none_or(MicroVmVirtioFsProfile::direct_io)
    {
        FOPEN_DIRECT_IO
    } else {
        0
    }
}

pub(crate) fn configure_session(fs: &VirtioFs, info: &mut SessionInfo) {
    if let Some(profile) = fs.inner.microvm_profile.as_ref() {
        let policy = profile.fuse_negotiation();
        // Session has already selected its supported protocol version;
        // make the profile-controlled portion of the response explicit.
        info.max_write = policy.maximum_write();
    }
}

pub(crate) fn check_symlink_allowed(fs: &VirtioFs) -> lx::Result<()> {
    // The generic LxVolume API cannot pin every ancestor while resolving
    // a symlink. The microVM profile therefore does not create links that
    // could later turn a checked relative lookup into an escape.
    if fs.is_microvm() {
        return Err(lx::Error::ENOTSUP);
    }
    Ok(())
}

pub(crate) fn insert_inode(
    fs: &VirtioFs,
    inodes: &mut InodeMap,
    inode: VirtioFsInode,
) -> lx::Result<(Arc<VirtioFsInode>, u64)> {
    if fs.is_microvm() {
        inodes.insert_microvm(inode)
    } else {
        inodes.insert(inode)
    }
}

pub(crate) fn validate_file_insert(
    fs: &VirtioFs,
    files: &HandleMap<Arc<VirtioFsFile>>,
) -> lx::Result<()> {
    if fs.is_microvm() && (files.values.len() >= MAX_HANDLES || !files.can_insert()) {
        return Err(lx::Error::ENOSPC);
    }
    Ok(())
}

impl VirtioFs {
    /// Creates a filesystem attachment for the fixed microVM profile.
    ///
    /// `root_path` is deliberately consumed only while opening the attachment;
    /// it is not retained in the filesystem state or in a saved-state blob.
    pub fn new_microvm(
        root_path: impl AsRef<Path>,
        profile: MicroVmVirtioFsProfile,
    ) -> anyhow::Result<Self> {
        let root_path = root_path.as_ref();
        profile.validate_root_path(root_path)?;
        let mut mount_options = LxVolumeOptions::new();
        mount_options.readonly(profile.is_readonly()).sandbox(true);
        let volume = mount_options.new_volume(root_path)?;
        let denied_identities = profile
            .denied_paths()
            .iter()
            .map(|path| {
                volume
                    .lstat(path)
                    .map(|stat| (stat.device_nr, stat.inode_nr))
            })
            .collect::<lx::Result<Vec<_>>>()?;
        let mut inodes = InodeMap::new(false);
        let volume = Arc::new(VirtioFsVolume::new_with_strict_paths(
            volume,
            0,
            profile.is_readonly(),
            true,
            profile.denied_paths().to_vec(),
            denied_identities,
        ));
        let (root_inode, root_stat) = VirtioFsInode::new(Arc::clone(&volume), PathBuf::new())?;
        profile.validate_opened_root(root_path, &root_stat)?;
        if inodes.insert(root_inode)?.1 != FUSE_ROOT_ID {
            anyhow::bail!("microVM virtio-fs root received an invalid node ID");
        }
        Ok(Self {
            inner: Arc::new(VirtioFsInner {
                inodes: RwLock::new(inodes),
                files: RwLock::new(HandleMap::new()),
                mode: VirtioFsMode::Direct,
                microvm_profile: Some(profile),
                negotiation: RwLock::new(FuseNegotiation::default()),
            }),
        })
    }

    pub(crate) fn microvm_profile(&self) -> Option<&MicroVmVirtioFsProfile> {
        self.inner.microvm_profile.as_ref()
    }

    pub(crate) fn is_microvm(&self) -> bool {
        self.inner.microvm_profile.is_some()
    }

    pub(crate) fn preflight_inode_insert(&self, inode: &VirtioFsInode) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner.inodes.read().preflight_microvm_insert(inode)
        } else {
            Ok(())
        }
    }

    pub(crate) fn preflight_new_inode_path(&self, path: &Path) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner.inodes.read().preflight_microvm_new_inode(path)
        } else {
            Ok(())
        }
    }

    pub(crate) fn preflight_create_inode(
        &self,
        parent: &VirtioFsInode,
        name: &lx::LxStr,
        path: &Path,
    ) -> lx::Result<()> {
        if !self.is_microvm() {
            return Ok(());
        }
        match parent.lookup_child(name) {
            Ok((inode, _)) => self.preflight_inode_insert(&inode),
            Err(error) if error == lx::Error::ENOENT => self.preflight_new_inode_path(path),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn preflight_alias_add(&self, inode: &VirtioFsInode, path: &Path) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner
                .inodes
                .read()
                .preflight_microvm_alias_add(inode, path)
        } else {
            Ok(())
        }
    }

    pub(crate) fn preflight_rename_aliases(
        &self,
        volume_id: u32,
        old: &Path,
        new: &Path,
    ) -> lx::Result<()> {
        if self.is_microvm() {
            self.inner
                .inodes
                .read()
                .preflight_microvm_rename(volume_id, old, new)
        } else {
            Ok(())
        }
    }

    pub(crate) fn preflight_file_insert(&self) -> lx::Result<()> {
        let files = self.inner.files.read();
        validate_file_insert(self, &files)
    }

    pub(crate) fn save_microvm_state(
        &self,
        profile: &MicroVmVirtioFsProfile,
        session_state: SessionState,
    ) -> anyhow::Result<SavedState> {
        anyhow::ensure!(
            self.microvm_profile() == Some(profile),
            "virtio-fs attachment does not match the microVM profile"
        );
        anyhow::ensure!(
            self.inner.aggregate().is_none(),
            "aggregate virtio-fs is not part of the microVM ABI"
        );

        let (inodes, node_ids, next_node_id) = {
            let inodes = self.inner.inodes.read();
            anyhow::ensure!(inodes.inodes_by_node_id.values.len() <= MAX_INODES);
            let mut saved = Vec::with_capacity(inodes.inodes_by_node_id.values.len());
            let mut node_ids = HashMap::with_capacity(inodes.inodes_by_node_id.values.len());
            let mut alias_count = 0usize;
            let mut alias_bytes = 0usize;
            for (&node_id, inode) in &inodes.inodes_by_node_id.values {
                let aliases = inode.aliases();
                anyhow::ensure!(
                    !aliases.is_empty() && aliases.len() <= MAX_ALIASES_PER_INODE,
                    "inode has no bounded reopenable aliases"
                );
                alias_count = alias_count
                    .checked_add(aliases.len())
                    .context("saved alias count overflow")?;
                anyhow::ensure!(alias_count <= MAX_ALIASES, "saved alias table is too large");
                let volume = inode.volume();
                let mut relative_aliases = Vec::with_capacity(aliases.len());
                let mut object_identity = None;
                for alias in aliases {
                    validate_relative_path(&alias, true)
                        .map_err(anyhow::Error::from)
                        .context("inode has an unsafe relative alias")?;
                    let stat = validate_reopenable_alias(&volume, &alias)
                        .context("inode alias cannot be revalidated for save")?;
                    let identity = saved_identity(&stat);
                    if let Some(expected) = &object_identity {
                        validate_saved_identity(&identity, expected)
                            .context("inode aliases identify different objects")?;
                    } else {
                        object_identity = Some(identity);
                    }
                    let alias = encode_relative_path(&alias)?;
                    alias_bytes = alias_bytes
                        .checked_add(alias.len())
                        .context("saved alias bytes overflow")?;
                    anyhow::ensure!(
                        alias_bytes <= MAX_ALIAS_BYTES,
                        "saved aliases exceed the aggregate byte bound"
                    );
                    relative_aliases.push(alias);
                }
                saved.push(SavedInode {
                    node_id,
                    volume_id: inode.volume_id(),
                    relative_aliases,
                    lookup_count: inode.lookup_count(),
                    guest_inode_id: inode.guest_inode_nr(),
                    object_identity: object_identity.context("inode has no aliases")?,
                });
                anyhow::ensure!(
                    node_ids
                        .insert(Arc::as_ptr(inode) as usize, node_id)
                        .is_none(),
                    "duplicate inode identity in inode table"
                );
            }
            (saved, node_ids, inodes.inodes_by_node_id.next_handle)
        };

        let handles = {
            let files = self.inner.files.read();
            anyhow::ensure!(files.values.len() <= MAX_HANDLES);
            let mut saved = Vec::with_capacity(files.values.len());
            let mut directory_entry_count = 0usize;
            let mut directory_bytes = 0usize;
            for (&handle_id, file) in &files.values {
                let node_id = *node_ids
                    .get(&(std::ptr::from_ref(file.inode()) as usize))
                    .context("open handle refers to an untracked inode")?;
                let stat = file
                    .object_stat()
                    .map_err(anyhow::Error::from)
                    .context("open handle cannot be revalidated for save")?;
                let snapshot_entries = file.directory_entries();
                let directory_snapshot_built = file.directory_snapshot_built();
                anyhow::ensure!(
                    directory_snapshot_built || snapshot_entries.is_empty(),
                    "uninitialized directory snapshot retained entries"
                );
                VirtioFsFile::validate_directory_entries(&snapshot_entries)
                    .map_err(anyhow::Error::from)
                    .context("directory enumeration state is invalid")?;
                directory_entry_count = directory_entry_count
                    .checked_add(snapshot_entries.len())
                    .context("saved directory entry count overflow")?;
                anyhow::ensure!(
                    directory_entry_count <= MAX_DIRECTORY_ENTRIES,
                    "saved directory entries exceed the aggregate bound"
                );
                for entry in &snapshot_entries {
                    directory_bytes = directory_bytes
                        .checked_add(entry.name.len())
                        .context("saved directory bytes overflow")?;
                }
                anyhow::ensure!(
                    directory_bytes <= MAX_DIRECTORY_BYTES,
                    "saved directory entries exceed the aggregate byte bound"
                );
                saved.push(SavedHandle {
                    handle_id,
                    node_id,
                    open_flags: file.open_flags(),
                    kind: stat.mode & lx::S_IFMT,
                    object_identity: saved_identity(&stat),
                    directory_entries: snapshot_entries,
                    directory_snapshot_built,
                });
            }
            saved
        };

        let negotiation = fuse_negotiation_from_session(session_state);
        anyhow::ensure!(
            *self.inner.negotiation.read() == negotiation,
            "FUSE session and filesystem negotiation state disagree"
        );
        Ok(SavedState {
            schema_version: SCHEMA_VERSION,
            attachment_id: profile.attachment_id().to_owned(),
            access_mode: match profile.access_mode() {
                super::profile::MicroVmAccessMode::ReadOnly => 1,
                super::profile::MicroVmAccessMode::ReadWrite => 2,
            },
            request_queues: profile.request_queues(),
            shared_memory_size: 0,
            packed_rings: false,
            direct_io: profile.direct_io(),
            entry_cache_timeout_ns: profile.entry_cache_timeout().as_nanos() as u64,
            attribute_cache_timeout_ns: profile.attribute_cache_timeout().as_nanos() as u64,
            negotiation: saved_negotiation(session_state),
            next_node_id,
            next_handle_id: self.inner.files.read().next_handle,
            inodes,
            handles,
            attachment_root_identity: profile.root_identity().to_vec(),
            maximum_request_size: MAX_FUSE_REQUEST_BYTES as u32,
            dormant: false,
        })
    }

    pub(crate) fn restore_microvm_state(
        &self,
        profile: &MicroVmVirtioFsProfile,
        state: SavedState,
        session: &Session,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.microvm_profile() == Some(profile),
            "virtio-fs attachment does not match the microVM profile"
        );
        validate_microvm_state(&state, profile)?;
        let session_state = session_state_from_saved(&state.negotiation)?;

        let current_root = self.get_inode(FUSE_ROOT_ID).map_err(anyhow::Error::from)?;
        let current_root_stat = current_root
            .object_stat()
            .map_err(anyhow::Error::from)
            .context("restore attachment root cannot be inspected")?;
        let saved_root = state
            .inodes
            .iter()
            .find(|inode| inode.node_id == FUSE_ROOT_ID)
            .context("saved state does not contain a root inode")?;
        validate_identity(&current_root_stat, &saved_root.object_identity)
            .context("restore attachment root identity does not match")?;
        let volume = current_root.volume();

        let mut restored_inodes = InodeMap::new(false);
        let mut node_ids = HashSet::with_capacity(state.inodes.len());
        let mut alias_paths = HashSet::new();
        for saved in &state.inodes {
            anyhow::ensure!(node_ids.insert(saved.node_id), "duplicate saved inode ID");
            anyhow::ensure!(saved.volume_id == 0, "unexpected saved volume ID");
            let mut aliases = Vec::with_capacity(saved.relative_aliases.len());
            let mut first_stat = None;
            for saved_alias in &saved.relative_aliases {
                let path = super::state::decode_relative_path(saved_alias)?;
                validate_relative_path(&path, true).map_err(anyhow::Error::from)?;
                if saved.node_id == FUSE_ROOT_ID {
                    anyhow::ensure!(
                        path.as_os_str().is_empty(),
                        "root inode has a non-root alias"
                    );
                } else {
                    anyhow::ensure!(
                        !path.as_os_str().is_empty(),
                        "non-root inode has a root alias"
                    );
                }
                anyhow::ensure!(
                    alias_paths.insert(path.clone()),
                    "saved aliases are duplicated or ambiguous"
                );
                let stat = validate_reopenable_alias(&volume, &path)
                    .context("saved inode alias cannot be reopened")?;
                validate_identity(&stat, &saved.object_identity)
                    .context("saved inode identity does not match the attachment")?;
                if first_stat.is_none() {
                    first_stat = Some(stat);
                }
                aliases.push(path);
            }
            let stat = first_stat.context("saved inode has no reopenable aliases")?;
            let inode =
                VirtioFsInode::from_saved(Arc::clone(&volume), aliases, saved.lookup_count, &stat)
                    .map_err(anyhow::Error::from)?;
            anyhow::ensure!(
                inode.guest_inode_nr() == saved.guest_inode_id,
                "saved guest inode ID does not match the attachment"
            );
            let key = inode.dedup_key();
            anyhow::ensure!(
                !restored_inodes.inodes_by_key.contains_key(&key),
                "saved inode aliases are ambiguous"
            );
            let inode = Arc::new(inode);
            restored_inodes
                .inodes_by_node_id
                .values
                .insert(saved.node_id, Arc::clone(&inode));
            restored_inodes
                .inodes_by_key
                .insert(key, (inode, saved.node_id));
        }
        restored_inodes.inodes_by_node_id.next_handle = state.next_node_id;

        let mut restored_files = HandleMap::starting_at(state.next_handle_id);
        let mut handle_ids = HashSet::with_capacity(state.handles.len());
        for saved in &state.handles {
            anyhow::ensure!(
                handle_ids.insert(saved.handle_id),
                "duplicate saved handle ID"
            );
            let inode = restored_inodes
                .get(saved.node_id)
                .context("saved handle refers to an unknown inode")?;
            let reopen_flags = reopen_flags(saved.open_flags)?;
            let file = inode
                .open(reopen_flags)
                .map_err(anyhow::Error::from)
                .context("saved handle cannot be reopened")?;
            let stat = file
                .object_stat()
                .map_err(anyhow::Error::from)
                .context("reopened handle cannot be inspected")?;
            validate_identity(&stat, &saved.object_identity)
                .context("reopened handle identity does not match")?;
            anyhow::ensure!(
                stat.mode & lx::S_IFMT == saved.kind,
                "reopened handle kind does not match"
            );
            file.restore_directory_snapshot(
                saved.directory_snapshot_built,
                saved.directory_entries.clone(),
            )
            .map_err(anyhow::Error::from)
            .context("saved directory snapshot is invalid")?;
            restored_files
                .values
                .insert(saved.handle_id, Arc::new(file));
        }

        session
            .restore_state(session_state)
            .map_err(anyhow::Error::from)
            .context("saved FUSE negotiation cannot be restored")?;
        *self.inner.inodes.write() = restored_inodes;
        *self.inner.files.write() = restored_files;
        *self.inner.negotiation.write() = fuse_negotiation_from_session(session_state);
        Ok(())
    }
}
