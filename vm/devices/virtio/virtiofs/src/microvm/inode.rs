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
    ) -> Self {
        Self {
            volume: Arc::new(volume),
            id,
            readonly,
            strict_paths,
        }
    }

    pub(crate) fn strict_paths(&self) -> bool {
        self.strict_paths
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
        let mut inode = Self::with_attr(volume, path, stat);
        inode.lookup_count = AtomicU64::new(lookup_count);
        let aliases: BTreeSet<_> = aliases.into_iter().collect();
        if aliases.is_empty() {
            return Err(lx::Error::EINVAL);
        }
        *inode.aliases.write() = aliases;
        Ok(inode)
    }

    /// Checks that a microVM path is relative and has no symlink component.
    ///
    /// LxVolume intentionally does not promise this property for arbitrary
    /// callers, so the profile applies a conservative check before each
    /// namespace operation. A handle-relative, race-free traversal primitive
    /// remains necessary for a complete cross-platform guarantee.
    pub(crate) fn validate_confined(&self) -> lx::Result<()> {
        if !self.volume.strict_paths() {
            return Ok(());
        }
        for path in self.aliases() {
            validate_relative_path(&path, true)?;
            let mut prefix = PathBuf::new();
            for component in path.components() {
                let Component::Normal(component) = component else {
                    return Err(lx::Error::EINVAL);
                };
                prefix.push(component);
                let stat = self.volume.lstat(&prefix)?;
                if stat.mode & lx::S_IFMT == lx::S_IFLNK {
                    return Err(lx::Error::ELOOP);
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn child_name_disallowed(volume: &VirtioFsVolume, name: &[u8]) -> bool {
    volume.strict_paths() && (name.contains(&b'\\') || name.contains(&b':'))
}

pub(crate) fn validate_child_path(volume: &VirtioFsVolume, path: &Path) -> lx::Result<()> {
    validate_relative_path(path, volume.strict_paths())?;
    if volume.strict_paths() && relative_path_encoded_len(path)? > MAX_PATH_BYTES {
        return Err(lx::Error::E2BIG);
    }
    Ok(())
}
