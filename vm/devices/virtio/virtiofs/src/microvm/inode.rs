// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM path confinement.

use super::saved_state::MAX_PATH_BYTES;
use super::state::relative_path_encoded_len;
use super::state::validate_relative_path;
use crate::inode::VirtioFsInode;
use crate::inode::VirtioFsVolume;
use lxutil::LxVolume;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

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
