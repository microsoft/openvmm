// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM filesystem construction and policy hooks.

use super::profile::MicroVmVirtioFsProfile;
use super::saved_state::MAX_HANDLES;
use crate::ATTRIBUTE_TIMEOUT;
use crate::ENTRY_TIMEOUT;
use crate::HandleMap;
use crate::InodeMap;
use crate::VirtioFs;
use crate::VirtioFsInner;
use crate::VirtioFsMode;
use crate::file::VirtioFsFile;
use crate::inode::VirtioFsInode;
use crate::inode::VirtioFsVolume;
use fuse::SessionInfo;
use fuse::protocol::FOPEN_DIRECT_IO;
use fuse::protocol::FUSE_ROOT_ID;
use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
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
    /// it is not retained in the filesystem state.
    pub fn new_microvm(
        root_path: impl AsRef<Path>,
        profile: MicroVmVirtioFsProfile,
    ) -> anyhow::Result<Self> {
        let root_path = root_path.as_ref();
        profile.validate_root_path(root_path)?;
        let mut mount_options = LxVolumeOptions::new();
        mount_options.readonly(profile.is_readonly()).sandbox(true);
        let volume = mount_options.new_volume(root_path)?;
        let mut inodes = InodeMap::new(false);
        let volume = Arc::new(VirtioFsVolume::new_with_strict_paths(
            volume,
            0,
            profile.is_readonly(),
            true,
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
}
