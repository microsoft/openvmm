// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Aggregate (multi-root) virtio-fs.
//!
//! An aggregate device exposes a synthetic, read-only root directory whose
//! named children are independent host folders ("roots"). Each child is
//! advertised to the guest with `FUSE_ATTR_SUBMOUNT` (when negotiated) so it
//! gets its own `st_dev`. This module owns all of the aggregate-only state and
//! the [`VirtioFs`] methods that operate on it; the core (direct-mode) file
//! system lives in the crate root.

use crate::ATTRIBUTE_TIMEOUT;
use crate::ENTRY_TIMEOUT;
use crate::VirtioFs;
use crate::inode;
use crate::inode::VirtioFsInode;
use fuse::DirEntryWriter;
use fuse::protocol::*;
use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use zerocopy::FromZeros;

/// Reserved file handle returned by `open_dir` on the synthetic aggregate root.
///
/// `read_dir`/`read_dir_plus`/`release_dir` recognize this sentinel and service
/// it from the root registry rather than the (real-file) handle map. `u64::MAX`
/// can never collide with a real handle because `HandleMap` allocates starting
/// at 1 and only increments.
pub(crate) const SYNTHETIC_ROOT_FH: u64 = u64::MAX;

/// Linux `DT_DIR` directory-entry type, used for the synthetic root's children.
const DT_DIR: u32 = 4;

/// A single host folder exposed as a named child of the synthetic aggregate root.
struct RootEntry {
    /// Name of the synthetic top-level directory. Chosen by the caller; the guest
    /// bind-mounts `<aggregate-mount>/<name>` onto the user's target path.
    name: String,
    volume: Arc<lxutil::LxVolume>,
    /// Stable identifier disambiguating per-volume inode numbers.
    volume_id: u32,
}

/// Registry of aggregated roots for an aggregate-mode [`VirtioFs`].
struct RootRegistry {
    entries: Vec<RootEntry>,
    next_volume_id: u32,
}

impl RootRegistry {
    fn new() -> Self {
        // Volume id 0 is reserved for a direct-mode single root, so aggregated
        // roots start at 1.
        Self {
            entries: Vec::new(),
            next_volume_id: 1,
        }
    }
}

/// State that only exists for an aggregate-mode [`VirtioFs`].
///
/// When present, node 1 is a synthetic directory whose children are the entries
/// in `roots`; when absent (direct mode), node 1 is a real inode at a single
/// volume root (legacy single-share behavior).
pub(crate) struct AggregateState {
    /// Aggregated roots exposed as named children of the synthetic root.
    roots: RwLock<RootRegistry>,
    /// When true, child roots are advertised with `FUSE_ATTR_SUBMOUNT` so the
    /// guest kernel gives each share its own `st_dev`. Only honored once
    /// `FUSE_SUBMOUNTS` is negotiated.
    submounts: bool,
    /// Set once the owning device host begins tearing the aggregate down (see
    /// [`VirtioFs::begin_teardown`]). After this, [`VirtioFs::add_root`] fails
    /// fast with `EAGAIN` rather than appending a root to a doomed device.
    tearing_down: AtomicBool,
}

impl AggregateState {
    pub(crate) fn new(submounts: bool) -> Self {
        Self {
            roots: RwLock::new(RootRegistry::new()),
            submounts,
            tearing_down: AtomicBool::new(false),
        }
    }
}

/// Aggregate-mode operations on [`VirtioFs`]. The crate-root `Fuse`
/// implementation dispatches the synthetic-root cases to the `pub(crate)`
/// helpers here.
impl VirtioFs {
    /// Expose a host folder as a named child of the synthetic root.
    ///
    /// Only valid in aggregate mode. Returns:
    /// - `EINVAL` on a direct-mode file system, or if the root's `readonly`
    ///   setting does not match the aggregate's (every child must agree, since
    ///   write permission is enforced device-wide).
    /// - `EAGAIN` if the device has begun tearing down (see
    ///   [`Self::begin_teardown`]).
    /// - `EEXIST` if a root with the same name already exists.
    pub fn add_root(
        &self,
        name: &str,
        root_path: impl AsRef<Path>,
        mount_options: Option<&LxVolumeOptions>,
    ) -> lx::Result<()> {
        let Some(aggregate) = &self.inner.aggregate else {
            return Err(lx::Error::EINVAL);
        };

        // Every child must share the aggregate's readonly setting: write
        // permission is checked against the device-wide `readonly` flag, so a
        // mismatched root would be silently mis-enforced.
        let child_readonly = mount_options.is_some_and(|o| o.is_readonly());
        if child_readonly != self.inner.readonly {
            return Err(lx::Error::EINVAL);
        }

        // Fast-fail before paying for volume construction if the device is
        // already tearing down. Re-checked under the lock below to close the
        // race with a concurrent `begin_teardown`.
        if aggregate.tearing_down.load(Ordering::Acquire) {
            return Err(lx::Error::EAGAIN);
        }

        let mut roots = aggregate.roots.write();
        if aggregate.tearing_down.load(Ordering::Acquire) {
            return Err(lx::Error::EAGAIN);
        }
        if roots.entries.iter().any(|e| e.name == name) {
            return Err(lx::Error::EEXIST);
        }

        let volume = if let Some(mount_options) = mount_options {
            mount_options.new_volume(root_path)
        } else {
            lxutil::LxVolume::new(root_path)
        }?;
        let volume_id = roots.next_volume_id;
        roots.next_volume_id += 1;
        roots.entries.push(RootEntry {
            name: name.to_string(),
            volume: Arc::new(volume),
            volume_id,
        });
        Ok(())
    }

    /// Signal that the owning device host has begun tearing the aggregate
    /// device down. After this, [`Self::add_root`] rejects new roots with
    /// `EAGAIN`, so an in-flight add cannot append a root to a device that is
    /// going away. No-op for direct-mode file systems.
    ///
    /// The running device keeps serving existing inodes until it is fully
    /// dropped; this only stops further roots from being added.
    pub fn begin_teardown(&self) {
        if let Some(aggregate) = &self.inner.aggregate {
            aggregate.tearing_down.store(true, Ordering::Release);
        }
    }

    /// Remove a previously added root by name.
    ///
    /// In-flight inodes beneath the root remain valid until the guest forgets
    /// them (each holds its own volume reference); the name simply stops
    /// appearing in the synthetic root. Returns `ENOENT` if no such root exists.
    pub fn remove_root(&self, name: &str) -> lx::Result<()> {
        let Some(aggregate) = &self.inner.aggregate else {
            return Err(lx::Error::EINVAL);
        };

        let mut roots = aggregate.roots.write();
        let before = roots.entries.len();
        roots.entries.retain(|e| e.name != name);
        if roots.entries.len() == before {
            Err(lx::Error::ENOENT)
        } else {
            Ok(())
        }
    }

    /// Returns true if `node_id` refers to the synthetic aggregate root.
    pub(crate) fn is_synthetic_root(&self, node_id: u64) -> bool {
        self.inner.aggregate.is_some() && node_id == FUSE_ROOT_ID
    }

    /// Whether aggregate children should be advertised with
    /// `FUSE_ATTR_SUBMOUNT`. Always false in direct mode.
    pub(crate) fn submounts(&self) -> bool {
        self.inner.aggregate.as_ref().is_some_and(|a| a.submounts)
    }

    /// Attributes of the synthetic aggregate root directory.
    pub(crate) fn synthetic_root_attr() -> fuse_attr {
        let mut attr = fuse_attr::new_zeroed();
        attr.ino = FUSE_ROOT_ID;
        attr.mode = lx::S_IFDIR | 0o555;
        attr.nlink = 2;
        attr.blksize = 512;
        attr
    }

    /// Extended attributes of the synthetic aggregate root directory.
    pub(crate) fn synthetic_root_statx(mask: lx::StatExMask) -> fuse_statx {
        let mut sx = fuse_statx::new_zeroed();
        sx.mask = mask.into_bits();
        sx.mode = (lx::S_IFDIR | 0o555) as u16;
        sx.nlink = 2;
        sx.ino = FUSE_ROOT_ID;
        sx.blksize = 512;
        sx
    }

    /// Looks up a named child of the synthetic root, returning an entry for the
    /// corresponding volume's real root inode.
    pub(crate) fn lookup_synthetic_root(&self, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        let Some(aggregate) = &self.inner.aggregate else {
            return Err(lx::Error::ENOENT);
        };
        let name_bytes = name.as_bytes();
        let (volume, volume_id) = {
            let roots = aggregate.roots.read();
            let entry = roots
                .entries
                .iter()
                .find(|e| e.name.as_bytes() == name_bytes)
                .ok_or(lx::Error::ENOENT)?;
            (Arc::clone(&entry.volume), entry.volume_id)
        };

        let (inode, stat) = VirtioFsInode::new(volume, volume_id, PathBuf::new())?;
        let mut attr = inode.attr_from_stat(&stat);
        let (_, node_id) = self.insert_inode(inode);
        if self.submounts() {
            attr.flags |= FUSE_ATTR_SUBMOUNT;
        }
        Ok(fuse_entry_out::new(
            node_id,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    /// Reads the synthetic root directory, listing `.`, `..`, and each root.
    pub(crate) fn read_synthetic_root_dir(
        &self,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        let Some(aggregate) = &self.inner.aggregate else {
            return Ok(Vec::new());
        };
        let mut buffer = Vec::with_capacity(size as usize);
        // `offset` is the cookie of the next entry to emit (0 at start of stream).
        // Entry 0 => ".", 1 => "..", 2.. => roots[index - 2].
        let mut index = offset;
        loop {
            let next = index + 1;
            let fit = match index {
                0 => self.write_synthetic_dot(&mut buffer, ".", next, plus),
                1 => self.write_synthetic_dot(&mut buffer, "..", next, plus),
                n => {
                    let root = {
                        let roots = aggregate.roots.read();
                        roots
                            .entries
                            .get((n - 2) as usize)
                            .map(|e| (e.name.clone(), Arc::clone(&e.volume), e.volume_id))
                    };
                    let Some((name, volume, volume_id)) = root else {
                        break;
                    };
                    self.write_root_entry(&mut buffer, &name, volume, volume_id, next, plus)?
                }
            };
            if !fit {
                break;
            }
            index += 1;
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
            let mut entry = fuse_entry_out::new_zeroed();
            entry.attr.ino = FUSE_ROOT_ID;
            entry.attr.mode = lx::S_IFDIR | 0o555;
            buffer.dir_entry_plus(name, next_off, entry)
        } else {
            buffer.dir_entry(name, FUSE_ROOT_ID, next_off, DT_DIR)
        }
    }

    /// Writes a directory entry for an aggregated root.
    fn write_root_entry(
        &self,
        buffer: &mut Vec<u8>,
        name: &str,
        volume: Arc<lxutil::LxVolume>,
        volume_id: u32,
        next_off: u64,
        plus: bool,
    ) -> lx::Result<bool> {
        if plus {
            if !buffer.check_dir_entry_plus(name) {
                return Ok(false);
            }
            // readdirplus performs a lookup on each entry, incrementing its
            // lookup count, so create/insert the root inode here.
            let (inode, stat) = VirtioFsInode::new(volume, volume_id, PathBuf::new())?;
            let mut attr = inode.attr_from_stat(&stat);
            let (_, node_id) = self.insert_inode(inode);
            if self.submounts() {
                attr.flags |= FUSE_ATTR_SUBMOUNT;
            }
            let entry = fuse_entry_out::new(node_id, ENTRY_TIMEOUT, ATTRIBUTE_TIMEOUT, attr);
            Ok(buffer.dir_entry_plus(name, next_off, entry))
        } else {
            // Plain readdir: report the directory using the volume root's real
            // inode number (namespaced to its volume), falling back to the
            // volume id if it is inaccessible.
            let raw = volume
                .lstat(&PathBuf::new())
                .map(|s| s.inode_nr)
                .unwrap_or(volume_id as lx::ino_t);
            let ino = inode::namespace_ino(volume_id, raw);
            Ok(buffer.dir_entry(name, ino, next_off, DT_DIR))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::VirtioFs;
    use crate::inode;
    use lxutil::LxVolumeOptions;

    #[test]
    fn aggregate_root_registry() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new_aggregate(false, true);

        fs.add_root("share_a", a.path(), None).unwrap();
        fs.add_root("share_b", b.path(), None).unwrap();

        // Duplicate names are rejected.
        assert_eq!(
            fs.add_root("share_a", a.path(), None).unwrap_err(),
            lx::Error::EEXIST
        );

        // Each root gets a distinct, non-zero volume id (0 is reserved for
        // direct mode).
        {
            let aggregate = fs.inner.aggregate.as_ref().unwrap();
            let roots = aggregate.roots.read();
            assert_eq!(roots.entries.len(), 2);
            assert_ne!(roots.entries[0].volume_id, 0);
            assert_ne!(roots.entries[0].volume_id, roots.entries[1].volume_id);
        }

        // Removal drops only the named root.
        fs.remove_root("share_a").unwrap();
        assert_eq!(fs.remove_root("share_a").unwrap_err(), lx::Error::ENOENT);
        assert_eq!(
            fs.inner
                .aggregate
                .as_ref()
                .unwrap()
                .roots
                .read()
                .entries
                .len(),
            1
        );
    }

    #[test]
    fn add_root_rejected_in_direct_mode() {
        let a = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new(a.path(), None).unwrap();
        assert_eq!(
            fs.add_root("x", a.path(), None).unwrap_err(),
            lx::Error::EINVAL
        );
        assert_eq!(fs.remove_root("x").unwrap_err(), lx::Error::EINVAL);
    }

    #[test]
    fn synthetic_root_node_ids_start_after_root() {
        // In aggregate mode the synthetic root occupies FUSE_ROOT_ID, so the
        // first real inode inserted must be allocated a higher id.
        let a = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new_aggregate(false, false);
        fs.add_root("share", a.path(), None).unwrap();
        let entry = fs
            .lookup_synthetic_root(lx::LxStr::from_bytes(b"share"))
            .unwrap();
        assert!(entry.nodeid > fuse::protocol::FUSE_ROOT_ID);
    }

    #[test]
    fn inode_namespacing_avoids_cross_volume_collisions() {
        // Direct mode (volume id 0) is the identity transform.
        assert_eq!(inode::namespace_ino(0, 42), 42);
        assert_eq!(inode::namespace_ino(0, 0), 0);

        // The same raw inode number on two different volumes maps to two
        // different reported numbers, so siblings never alias.
        let raw = 2; // e.g. the root inode of two freshly-formatted volumes
        assert_ne!(
            inode::namespace_ino(1, raw),
            inode::namespace_ino(2, raw),
            "sibling volumes must not collide"
        );

        // Within a single volume the transform is a bijection, so distinct
        // files keep distinct inode numbers (preserving hard-link identity).
        assert_ne!(inode::namespace_ino(1, 10), inode::namespace_ino(1, 11));
    }

    #[test]
    fn add_root_enforces_uniform_readonly() {
        let a = tempfile::tempdir().unwrap();
        let mut ro = LxVolumeOptions::default();
        ro.readonly(true);
        let mut rw = LxVolumeOptions::default();
        rw.readonly(false);

        // A read-write aggregate rejects a readonly child.
        let rw_fs = VirtioFs::new_aggregate(false, false);
        assert_eq!(
            rw_fs.add_root("ro_child", a.path(), Some(&ro)).unwrap_err(),
            lx::Error::EINVAL
        );
        rw_fs.add_root("rw_child", a.path(), Some(&rw)).unwrap();

        // A readonly aggregate rejects a read-write child.
        let ro_fs = VirtioFs::new_aggregate(true, false);
        assert_eq!(
            ro_fs.add_root("rw_child", a.path(), Some(&rw)).unwrap_err(),
            lx::Error::EINVAL
        );
        ro_fs.add_root("ro_child", a.path(), Some(&ro)).unwrap();
    }

    #[test]
    fn add_root_rejected_after_teardown() {
        let a = tempfile::tempdir().unwrap();
        let fs = VirtioFs::new_aggregate(false, false);
        fs.add_root("before", a.path(), None).unwrap();

        fs.begin_teardown();

        // Once tearing down, no further roots can be added.
        assert_eq!(
            fs.add_root("after", a.path(), None).unwrap_err(),
            lx::Error::EAGAIN
        );
        assert_eq!(
            fs.inner
                .aggregate
                .as_ref()
                .unwrap()
                .roots
                .read()
                .entries
                .len(),
            1
        );
    }
}
