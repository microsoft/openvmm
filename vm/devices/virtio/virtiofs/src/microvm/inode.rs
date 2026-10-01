// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM path confinement and persisted inode reconstruction.

use super::saved_state::MAX_PATH_BYTES;
use super::state::relative_path_encoded_len;
use super::state::validate_relative_path;
use crate::inode::VirtioFsInode;
use crate::inode::VirtioFsVolume;
use lxutil::LxVolume;
use std::collections::BTreeSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

impl VirtioFsVolume {
    pub(crate) fn new_with_strict_paths(
        volume: LxVolume,
        id: u32,
        readonly: bool,
        strict_paths: bool,
        denied_paths: Vec<PathBuf>,
        denied_identities: Vec<(u64, u64)>,
    ) -> Self {
        Self {
            volume: Arc::new(volume),
            id,
            readonly,
            strict_paths,
            denied_paths,
            denied_identities,
        }
    }

    pub(crate) fn strict_paths(&self) -> bool {
        self.strict_paths
    }

    pub(crate) fn ensure_path_allowed(&self, path: &Path) -> lx::Result<()> {
        if self
            .denied_paths
            .iter()
            .any(|denied| path.starts_with(denied))
        {
            return Err(lx::Error::EACCES);
        }
        Ok(())
    }

    pub(crate) fn ensure_identity_allowed(&self, stat: &lx::Stat) -> lx::Result<()> {
        if self
            .denied_identities
            .contains(&(stat.device_nr, stat.inode_nr))
        {
            return Err(lx::Error::EACCES);
        }
        Ok(())
    }
}

impl VirtioFsInode {
    /// Rebuilds an inode after a saved attachment identity has been
    /// independently revalidated.
    pub(crate) fn from_saved(
        volume: Arc<VirtioFsVolume>,
        aliases: Vec<PathBuf>,
        lookup_count: u64,
        stat: &lx::Stat,
    ) -> lx::Result<Self> {
        let Some(path) = aliases.first().cloned() else {
            return Err(lx::Error::EINVAL);
        };
        if lookup_count == 0 {
            return Err(lx::Error::EINVAL);
        }
        volume.ensure_identity_allowed(stat)?;
        let mut inode = Self::with_attr(volume, path, stat);
        inode.lookup_count = AtomicU64::new(lookup_count);
        let aliases: BTreeSet<_> = aliases.into_iter().collect();
        if aliases.is_empty() {
            return Err(lx::Error::EINVAL);
        }
        *inode.aliases.write() = aliases;
        Ok(inode)
    }

    /// Checks that a microVM inode used as a directory is reached without
    /// crossing a symbolic link, including at its final component.
    ///
    /// LxVolume intentionally does not promise this property for arbitrary
    /// callers, so the profile applies a conservative check before each
    /// namespace operation. On Linux, the confined volume additionally
    /// resolves every host path without following a symbolic link, so this
    /// check is not the only defense against a concurrent rename. Windows
    /// guests can only create WSL-style links, which Windows path resolution
    /// never follows.
    pub(crate) fn validate_confined(&self) -> lx::Result<()> {
        self.validate_confined_paths(false)
    }

    /// Checks that a microVM inode is reached without crossing a symbolic
    /// link. The inode itself may be a symbolic link, which operations on the
    /// object never follow.
    pub(crate) fn validate_confined_object(&self) -> lx::Result<()> {
        self.validate_confined_paths(true)
    }

    fn validate_confined_paths(&self, allow_final_link: bool) -> lx::Result<()> {
        if !self.volume.strict_paths() {
            return Ok(());
        }
        // Operations use the primary path, which outlives the aliases when the
        // object is unlinked, so it must be validated as well.
        let mut paths = self.aliases();
        let primary = self.clone_path();
        if !paths.contains(&primary) {
            paths.push(primary);
        }
        for path in paths {
            validate_relative_path(&path, true)?;
            self.volume.ensure_path_allowed(&path)?;
            let component_count = path.components().count();
            let mut prefix = PathBuf::new();
            for (index, component) in path.components().enumerate() {
                let Component::Normal(component) = component else {
                    return Err(lx::Error::EINVAL);
                };
                prefix.push(component);
                let stat = self.volume.lstat(&prefix)?;
                let final_link = allow_final_link && index + 1 == component_count;
                if stat.mode & lx::S_IFMT == lx::S_IFLNK && !final_link {
                    return Err(lx::Error::ELOOP);
                }
            }
        }
        Ok(())
    }
}

/// Rejects a symbolic link opened on a strict volume.
///
/// Windows opens a link itself when asked not to follow it, so the opened
/// object is checked rather than relying on the host to refuse.
pub(crate) fn reject_opened_symlink(
    volume: &VirtioFsVolume,
    file: &lxutil::LxFile,
) -> lx::Result<()> {
    if volume.strict_paths() {
        reject_symlink_stat(volume, &file.fstat()?.into())?;
    }
    Ok(())
}

/// Rejects the attributes of an opened symbolic link on a strict volume.
pub(crate) fn reject_symlink_stat(volume: &VirtioFsVolume, stat: &lx::Stat) -> lx::Result<()> {
    if volume.strict_paths() && stat.mode & lx::S_IFMT == lx::S_IFLNK {
        return Err(lx::Error::ELOOP);
    }
    Ok(())
}

pub(crate) fn child_name_disallowed(volume: &VirtioFsVolume, name: &[u8]) -> bool {
    volume.strict_paths() && (name.contains(&b'\\') || name.contains(&b':'))
}

pub(crate) fn validate_child_path(volume: &VirtioFsVolume, path: &Path) -> lx::Result<()> {
    validate_relative_path(path, volume.strict_paths())?;
    if volume.strict_paths() && relative_path_encoded_len(path)? > MAX_PATH_BYTES {
        return Err(lx::Error::E2BIG);
    }
    volume.ensure_path_allowed(path)
}
