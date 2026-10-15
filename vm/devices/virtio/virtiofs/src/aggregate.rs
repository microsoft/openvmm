// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aggregate (multi-root) virtio-fs.
//!
//! An aggregate device exposes a synthetic, read-only root directory whose
//! named children are independent host folders sharing a single FUSE
//! superblock. Each child's inode numbers are namespaced per volume (see
//! [`namespace_ino`](crate::inode::namespace_ino)) to avoid cross-volume
//! `st_ino` collisions. This module owns all of the aggregate-only state and
//! the [`VirtioFs`] methods that operate on it; the core (direct-mode) file
//! system lives in the crate root.

use crate::ATTRIBUTE_TIMEOUT;
use crate::ENTRY_TIMEOUT;
use crate::VirtioFs;
use crate::build_volume;
use crate::inode::VirtioFsInode;
use crate::inode::VirtioFsVolume;
use fuse::DirEntryWriter;
use fuse::check_name;
use fuse::protocol::*;
use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use zerocopy::FromZeros;

/// Reserved file handle `open_dir` returns for the synthetic aggregate root;
/// `read_dir`/`read_dir_plus`/`release_dir` recognize it and serve the root
/// registry instead of the handle map. `u64::MAX` can't collide with a real
/// handle, since `HandleMap` allocates from 1 upward.
pub(crate) const SYNTHETIC_ROOT_FH: u64 = u64::MAX;

/// A single host folder exposed as a named child of the synthetic aggregate root.
struct ChildEntry {
    /// Name of this child's directory under the synthetic root. Chosen by the
    /// caller; the guest bind-mounts `<aggregate-mount>/<name>` onto the user's
    /// target path.
    name: String,
    volume: Arc<VirtioFsVolume>,
}

/// Registry of aggregated children for an aggregate-mode [`VirtioFs`].
struct AggregateRegistry {
    entries: Vec<ChildEntry>,
    next_volume_id: u32,
    tearing_down: bool,
}

impl AggregateRegistry {
    fn new() -> Self {
        // Volume id 0 is reserved for a direct-mode single root, so aggregated
        // children start at 1.
        Self {
            entries: Vec::new(),
            next_volume_id: 1,
            tearing_down: false,
        }
    }

    fn check_can_add(&self) -> lx::Result<()> {
        if self.tearing_down {
            return Err(lx::Error::EAGAIN);
        }
        Ok(())
    }
}

/// State that only exists for an aggregate-mode [`VirtioFs`].
///
/// When present, node 1 is a synthetic directory whose children are the entries
/// in `registry`.
pub(crate) struct AggregateState {
    /// Aggregated children and their lifecycle state.
    registry: RwLock<AggregateRegistry>,
}

impl AggregateState {
    pub(crate) fn new() -> Self {
        Self {
            registry: RwLock::new(AggregateRegistry::new()),
        }
    }
}

/// Aggregate-mode operations on [`VirtioFs`]. The crate-root `Fuse`
/// implementation dispatches the synthetic-root cases to the `pub(crate)`
/// helpers here.
impl VirtioFs {
    /// Expose a host folder as a named child of the synthetic root.
    ///
    /// Each child carries its own read-only setting (from `mount_options`), so
    /// shares under one aggregate device may differ.
    ///
    /// Only valid in aggregate mode. Returns:
    /// - `EINVAL` on a direct-mode file system, or if `name` is empty,
    ///   reserved (`.`/`..`), or contains `/` or `\0`.
    /// - `EAGAIN` if the device has begun tearing down (see
    ///   [`Self::begin_teardown`]).
    /// - `EEXIST` if a child with the same name already exists.
    /// - `ENOSPC` if the volume-id space is exhausted (2^32 children).
    pub fn add_child(
        &self,
        name: &str,
        root_path: impl AsRef<Path>,
        mount_options: Option<&LxVolumeOptions>,
    ) -> lx::Result<()> {
        let Some(aggregate) = self.inner.aggregate() else {
            return Err(lx::Error::EINVAL);
        };

        check_name(name.as_bytes())?;

        // Fast-fail before paying for volume construction if the device is
        // already tearing down. Re-checked under the lock below to close the
        // race with a concurrent `begin_teardown`.
        {
            aggregate.registry.read().check_can_add()?;
        }

        let (volume, readonly) = build_volume(root_path, mount_options)?;

        let mut registry = aggregate.registry.write();
        registry.check_can_add()?;
        if registry.entries.iter().any(|e| e.name == name) {
            return Err(lx::Error::EEXIST);
        }

        let volume_id = registry.next_volume_id;
        registry.next_volume_id = volume_id.checked_add(1).ok_or(lx::Error::ENOSPC)?;
        registry.entries.push(ChildEntry {
            name: name.to_string(),
            volume: Arc::new(VirtioFsVolume::new(volume, volume_id, readonly)),
        });
        tracing::info!(
            name,
            volume_id,
            child_count = registry.entries.len(),
            "added aggregate virtio-fs child"
        );
        Ok(())
    }

    /// Signal that the aggregate device has begun tearing down, so
    /// [`Self::add_child`] rejects further children with `EAGAIN`. Existing
    /// inodes keep being served until the device is dropped. No-op in direct
    /// mode.
    pub fn begin_teardown(&self) {
        if let Some(aggregate) = self.inner.aggregate() {
            aggregate.registry.write().tearing_down = true;
        }
    }

    /// Remove a previously added child by name.
    ///
    /// In-flight inodes beneath the child remain valid until the guest forgets
    /// them (each holds its own volume reference); the name simply stops
    /// appearing in the synthetic root. Returns `ENOENT` if no such child exists.
    pub fn remove_child(&self, name: &str) -> lx::Result<()> {
        let Some(aggregate) = self.inner.aggregate() else {
            return Err(lx::Error::EINVAL);
        };

        let mut children = aggregate.registry.write();
        let before = children.entries.len();
        children.entries.retain(|e| e.name != name);
        if children.entries.len() == before {
            Err(lx::Error::ENOENT)
        } else {
            Ok(())
        }
    }

    /// Returns true if `node_id` refers to the synthetic aggregate root.
    pub(crate) fn is_synthetic_root(&self, node_id: u64) -> bool {
        self.inner.aggregate().is_some() && node_id == FUSE_ROOT_ID
    }

    pub(crate) fn is_synthetic_root_handle(&self, node_id: u64, fh: u64) -> bool {
        self.is_synthetic_root(node_id) && fh == SYNTHETIC_ROOT_FH
    }

    /// Attributes of the synthetic aggregate root directory.
    pub(crate) fn synthetic_root_attr(&self) -> fuse_attr {
        let mut attr = fuse_attr::new_zeroed();
        attr.ino = FUSE_ROOT_ID;
        attr.mode = lx::S_IFDIR | 0o555;
        attr.nlink = self.synthetic_root_nlink();
        attr.blksize = 512;
        attr
    }

    /// Extended attributes of the synthetic aggregate root directory.
    pub(crate) fn synthetic_root_statx(&self, mask: lx::StatExMask) -> fuse_statx {
        let mut sx = fuse_statx::new_zeroed();
        let returned_mask = lx::StatExMask::new()
            .with_file_type(true)
            .with_mode(true)
            .with_nlink(true)
            .with_ino(true)
            .into_bits();
        sx.mask = mask.into_bits() & returned_mask;
        sx.mode = (lx::S_IFDIR | 0o555) as u16;
        sx.nlink = self.synthetic_root_nlink();
        sx.ino = FUSE_ROOT_ID;
        sx.blksize = 512;
        sx
    }

    fn synthetic_root_nlink(&self) -> u32 {
        let child_count = self
            .inner
            .aggregate()
            .map_or(0, |aggregate| aggregate.registry.read().entries.len());
        u32::try_from(child_count)
            .unwrap_or(u32::MAX)
            .saturating_add(2)
    }

    /// Looks up a named child of the synthetic root, returning an entry for the
    /// corresponding volume's real root inode.
    pub(crate) fn lookup_synthetic_root(&self, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        let Some(aggregate) = self.inner.aggregate() else {
            return Err(lx::Error::ENOENT);
        };
        let name_bytes = name.as_bytes();
        let volume = {
            let children = aggregate.registry.read();
            let entry = children
                .entries
                .iter()
                .find(|e| e.name.as_bytes() == name_bytes)
                .ok_or(lx::Error::ENOENT)?;
            Arc::clone(&entry.volume)
        };

        self.insert_child_root_entry(volume)
    }

    fn insert_child_root_entry(&self, volume: Arc<VirtioFsVolume>) -> lx::Result<fuse_entry_out> {
        let (inode, stat) = VirtioFsInode::new(volume, PathBuf::new())?;
        let attr = inode.attr_from_stat(&stat);
        let (_, node_id) = self.insert_inode(inode);
        Ok(fuse_entry_out::new(
            node_id,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    /// Reads the synthetic root directory, listing `.`, `..`, and each child.
    pub(crate) fn read_synthetic_root_dir(
        &self,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        let Some(aggregate) = self.inner.aggregate() else {
            return Ok(Vec::new());
        };
        let mut buffer = Vec::with_capacity(size as usize);
        // `offset` is the cookie of the next entry to emit (0 at start of stream).
        // Entry 0 => ".", 1 => "..", 2.. => children[index - 2].
        let mut index = offset;
        while let Some(next) = index.checked_add(1) {
            let fit = match index {
                0 => self.write_synthetic_dot(&mut buffer, ".", next, plus),
                1 => self.write_synthetic_dot(&mut buffer, "..", next, plus),
                n => {
                    let child = {
                        let children = aggregate.registry.read();
                        children
                            .entries
                            .get((n - 2) as usize)
                            .map(|e| (e.name.clone(), Arc::clone(&e.volume)))
                    };
                    let Some((name, volume)) = child else {
                        break;
                    };
                    self.write_child_entry(&mut buffer, &name, volume, next, plus)?
                }
            };
            if !fit {
                break;
            }
            index = next;
        }
        Ok(buffer)
    }

    /// Writes a synthetic `.`/`..` entry. These never carry a real node ID, so
    /// the kernel will not issue a forget for them.
    fn write_synthetic_dot(
        &self,
        buffer: &mut Vec<u8>,
        name: &str,
        next_off: u64,
        plus: bool,
    ) -> bool {
        if plus {
            if !buffer.check_dir_entry_plus(name) {
                return false;
            }
            let entry = fuse_entry_out::new_dot(FUSE_ROOT_ID, lx::S_IFDIR | 0o555);
            buffer.dir_entry_plus(name, next_off, entry)
        } else {
            buffer.dir_entry(name, FUSE_ROOT_ID, next_off, lx::DT_DIR as u32)
        }
    }

    /// Writes a directory entry for an aggregated child.
    fn write_child_entry(
        &self,
        buffer: &mut Vec<u8>,
        name: &str,
        volume: Arc<VirtioFsVolume>,
        next_off: u64,
        plus: bool,
    ) -> lx::Result<bool> {
        if plus {
            if !buffer.check_dir_entry_plus(name) {
                return Ok(false);
            }
            // readdirplus performs a lookup on each entry, incrementing its
            // lookup count, so create/insert the root inode here.
            let entry = self.insert_child_root_entry(volume)?;
            Ok(buffer.dir_entry_plus(name, next_off, entry))
        } else {
            // Plain readdir: report the directory using the volume root's
            // guest-visible inode number. If the root cannot be queried, use
            // the volume id as a stable surrogate.
            let raw = volume
                .lstat(PathBuf::new())
                .map(|s| s.inode_nr)
                .unwrap_or(volume.id() as lx::ino_t);
            let ino = volume.map_inode(raw);
            Ok(buffer.dir_entry(name, ino, next_off, lx::DT_DIR as u32))
        }
    }
}

#[cfg(test)]
#[path = "aggregate_tests.rs"]
mod tests;
