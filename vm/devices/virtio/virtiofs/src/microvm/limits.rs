// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! microVM inode, alias, and path allocation limits.

use super::saved_state::MAX_ALIAS_BYTES;
use super::saved_state::MAX_ALIASES;
use super::saved_state::MAX_ALIASES_PER_INODE;
use super::saved_state::MAX_INODES;
use super::saved_state::MAX_PATH_BYTES;
use super::state::relative_path_encoded_len;
use super::state::validate_relative_path;
use crate::InodeMap;
use crate::inode::VirtioFsInode;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

impl InodeMap {
    pub(crate) fn insert_microvm(
        &mut self,
        inode: VirtioFsInode,
    ) -> lx::Result<(Arc<VirtioFsInode>, u64)> {
        self.preflight_microvm_insert(&inode)?;
        self.insert(inode)
    }

    pub(crate) fn preflight_microvm_insert(&self, inode: &VirtioFsInode) -> lx::Result<()> {
        let path = inode.clone_path();
        Self::validate_microvm_alias_path(&path)?;
        if let Some((existing, _)) = self.inodes_by_key.get(&inode.dedup_key()) {
            return self.preflight_microvm_alias_add(existing, &path);
        }
        self.preflight_microvm_new_inode(&path)
    }

    pub(crate) fn preflight_microvm_new_inode(&self, path: &Path) -> lx::Result<()> {
        Self::validate_microvm_alias_path(path)?;
        if self.inodes_by_node_id.values.len() >= MAX_INODES || !self.inodes_by_node_id.can_insert()
        {
            return Err(lx::Error::ENOSPC);
        }
        let (alias_count, alias_bytes) = self.microvm_alias_usage()?;
        if alias_count >= MAX_ALIASES {
            return Err(lx::Error::ENOSPC);
        }
        let path_bytes = relative_path_encoded_len(path)?;
        if alias_bytes
            .checked_add(path_bytes)
            .is_none_or(|bytes| bytes > MAX_ALIAS_BYTES)
        {
            return Err(lx::Error::E2BIG);
        }
        Ok(())
    }

    pub(crate) fn preflight_microvm_alias_add(
        &self,
        inode: &VirtioFsInode,
        path: &Path,
    ) -> lx::Result<()> {
        Self::validate_microvm_alias_path(path)?;
        let aliases = inode.aliases();
        if aliases.iter().any(|alias| alias == path) {
            return Ok(());
        }
        if aliases.len() >= MAX_ALIASES_PER_INODE {
            return Err(lx::Error::ENOSPC);
        }
        let (alias_count, alias_bytes) = self.microvm_alias_usage()?;
        if alias_count >= MAX_ALIASES {
            return Err(lx::Error::ENOSPC);
        }
        let path_bytes = relative_path_encoded_len(path)?;
        if alias_bytes
            .checked_add(path_bytes)
            .is_none_or(|bytes| bytes > MAX_ALIAS_BYTES)
        {
            return Err(lx::Error::E2BIG);
        }
        Ok(())
    }

    pub(crate) fn preflight_microvm_rename(
        &self,
        volume_id: u32,
        old: &Path,
        new: &Path,
    ) -> lx::Result<()> {
        Self::validate_microvm_alias_path(new)?;
        let mut alias_count = 0usize;
        let mut alias_bytes = 0usize;
        for inode in self.inodes_by_node_id.values.values() {
            let mut aliases: BTreeSet<_> = inode.aliases().into_iter().collect();
            if inode.volume_id() == volume_id {
                aliases.retain(|alias| !alias.starts_with(new));
                let replacements: Vec<_> = aliases
                    .iter()
                    .filter_map(|alias| {
                        alias.strip_prefix(old).ok().map(|suffix| {
                            let mut replacement = new.to_path_buf();
                            replacement.push(suffix);
                            (alias.clone(), replacement)
                        })
                    })
                    .collect();
                for (old_alias, new_alias) in replacements {
                    aliases.remove(&old_alias);
                    aliases.insert(new_alias);
                }
            }
            if aliases.len() > MAX_ALIASES_PER_INODE {
                return Err(lx::Error::ENOSPC);
            }
            alias_count = alias_count
                .checked_add(aliases.len())
                .ok_or(lx::Error::ENOSPC)?;
            if alias_count > MAX_ALIASES {
                return Err(lx::Error::ENOSPC);
            }
            for alias in aliases {
                Self::validate_microvm_alias_path(&alias)?;
                alias_bytes = alias_bytes
                    .checked_add(relative_path_encoded_len(&alias)?)
                    .ok_or(lx::Error::E2BIG)?;
                if alias_bytes > MAX_ALIAS_BYTES {
                    return Err(lx::Error::E2BIG);
                }
            }
        }
        Ok(())
    }

    fn microvm_alias_usage(&self) -> lx::Result<(usize, usize)> {
        let mut count = 0usize;
        let mut bytes = 0usize;
        for inode in self.inodes_by_node_id.values.values() {
            let aliases = inode.aliases();
            count = count.checked_add(aliases.len()).ok_or(lx::Error::ENOSPC)?;
            for alias in aliases {
                bytes = bytes
                    .checked_add(relative_path_encoded_len(&alias)?)
                    .ok_or(lx::Error::E2BIG)?;
            }
        }
        Ok((count, bytes))
    }

    fn validate_microvm_alias_path(path: &Path) -> lx::Result<()> {
        validate_relative_path(path, true)?;
        if relative_path_encoded_len(path)? > MAX_PATH_BYTES {
            return Err(lx::Error::E2BIG);
        }
        Ok(())
    }
}
