// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::file::VirtioFsFile;
use crate::util;
use fuse::protocol::*;
use lx::LxStr;
use lx::LxString;
use lxutil::LxCreateOptions;
use lxutil::LxVolume;
use lxutil::PathBufExt;
use parking_lot::RwLock;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Multiplier used to spread a volume id across the 64-bit inode space when
/// namespacing inode numbers (see [`namespace_ino`]). It is the 64-bit
/// golden-ratio constant, chosen because it has good bit-mixing properties.
const INO_NAMESPACE_MULTIPLIER: u64 = 0x9E37_79B9_7F4A_7C15;

/// XORs a per-volume key into a raw host inode number to reduce the likelihood
/// of cross-volume `st_ino` collisions under the shared FUSE superblock.
///
/// `volume_id == 0` (direct/single-root mode) returns `raw` unchanged. For
/// aggregated volumes the transform is a bijection within each volume
/// (preserving hard-link identity) and uses the full 64-bit inode space, so it
/// never overflows and imposes no limit on the number of volumes. Cross-volume
/// collisions are still possible when two volumes happen to have inode numbers
/// whose XOR equals the difference of their volume keys.
pub(crate) fn namespace_ino(volume_id: u32, raw: lx::ino_t) -> lx::ino_t {
    if volume_id == 0 {
        return raw;
    }
    raw ^ u64::from(volume_id).wrapping_mul(INO_NAMESPACE_MULTIPLIER)
}

pub(crate) struct VirtioFsVolume {
    volume: Arc<LxVolume>,
    id: u32,
    readonly: bool,
}

impl VirtioFsVolume {
    pub(crate) fn new(volume: LxVolume, id: u32, readonly: bool) -> Self {
        Self {
            volume: Arc::new(volume),
            id,
            readonly,
        }
    }

    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn readonly(&self) -> bool {
        self.readonly
    }

    pub(crate) fn map_inode(&self, raw: lx::ino_t) -> lx::ino_t {
        namespace_ino(self.id, raw)
    }
}

impl Deref for VirtioFsVolume {
    type Target = LxVolume;

    fn deref(&self) -> &Self::Target {
        &self.volume
    }
}

/// Key used to deduplicate inodes in the `InodeMap` so that repeated lookups of
/// the same host file return a single, stable FUSE node id.
///
/// Volumes that report stable inode numbers key by `(volume_id, inode_nr)`.
/// Volumes that recycle inode numbers (FAT/exFAT) cannot trust the inode
/// number as an identity, so they key by `(volume_id, path)` instead — a given
/// path then maps to one node id across lookups even though the host inode
/// number is not reliable.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum DedupKey {
    /// `(volume_id, host_inode_number)`, for stable-id volumes.
    Ino(u32, lx::ino_t),
    /// `(volume_id, host_path)`, for volumes that recycle inode numbers.
    Path(u32, PathBuf),
}

/// Implements inode callbacks for virtio-fs.
pub struct VirtioFsInode {
    pub(crate) volume: Arc<VirtioFsVolume>,
    path: RwLock<PathBuf>,
    lookup_count: AtomicU64,
    inode_nr: lx::ino_t,
    /// This inode's number as reported to the guest: its namespaced inode
    /// number under the shared superblock.
    guest_inode_nr: lx::ino_t,
}

impl VirtioFsInode {
    /// Create a new inode for the specified path.
    pub fn new(volume: Arc<VirtioFsVolume>, path: PathBuf) -> lx::Result<(Self, lx::Stat)> {
        let stat = volume.lstat(&path)?;
        let inode = Self::with_attr(volume, path, &stat);
        Ok((inode, stat))
    }

    /// Create a new inode for the specified path, with previously retrieved attributes.
    pub fn with_attr(volume: Arc<VirtioFsVolume>, path: PathBuf, stat: &lx::Stat) -> Self {
        let guest_inode_nr = volume.map_inode(stat.inode_nr);
        Self {
            volume,
            path: RwLock::new(path),
            lookup_count: AtomicU64::new(1),
            inode_nr: stat.inode_nr,
            guest_inode_nr,
        }
    }

    /// Return the files inode number as reported by the underlying file system.
    ///
    /// N.B. This may be different from its FUSE node ID.
    pub fn inode_nr(&self) -> lx::ino_t {
        self.inode_nr
    }

    /// Return the identifier of the aggregated volume this inode belongs to.
    pub fn volume_id(&self) -> u32 {
        self.volume.id()
    }

    /// Whether this inode's volume is read-only.
    pub fn readonly(&self) -> bool {
        self.volume.readonly()
    }

    /// This inode's own number as reported to the guest: its namespaced inode
    /// number under the shared superblock. Fixed for the inode's lifetime.
    pub(crate) fn guest_inode_nr(&self) -> lx::ino_t {
        self.guest_inode_nr
    }

    /// Maps a raw host inode number from this inode's volume for guest use. For
    /// numbers belonging to *other* inodes in the same volume (e.g. readdir
    /// child entries); for this inode's own number use [`Self::guest_inode_nr`].
    pub(crate) fn guest_ino(&self, raw: lx::ino_t) -> lx::ino_t {
        self.volume.map_inode(raw)
    }

    /// Builds a `fuse_attr` from a stat *of this inode*, reporting its cached
    /// guest-visible inode number.
    ///
    /// `stat` must describe this inode; to report a different inode's
    /// attributes (e.g. a hard-link target) call this on that inode.
    pub(crate) fn attr_from_stat(&self, stat: &lx::Stat) -> fuse_attr {
        let mut attr = util::stat_to_fuse_attr(stat);
        attr.ino = self.guest_inode_nr;
        attr
    }

    /// Builds a `fuse_statx` from a statx *of this inode*, reporting its cached
    /// guest-visible inode number.
    pub(crate) fn statx_from(&self, statx: &lx::StatEx) -> fuse_statx {
        let mut sx = util::statx_to_fuse_statx(statx);
        sx.ino = self.guest_inode_nr;
        sx
    }

    /// Increments the lookup count.
    pub fn lookup(&self, new_path: PathBuf) {
        self.lookup_count.fetch_add(1, Ordering::AcqRel);
        let mut path = self.path.write();
        *path = new_path;
    }

    /// Increments the lookup count without updating the path.
    ///
    /// This is used when returning an existing inode in a FUSE reply (e.g., for hard links)
    /// where the kernel will track the reference and later send a forget.
    pub fn inc_lookup(&self) {
        self.lookup_count.fetch_add(1, Ordering::AcqRel);
    }

    /// Decrements the lookup count, and returns the new count.
    pub fn forget(&self, node_id: u64, lookup_count: u64) -> u64 {
        let mut old_count = self.lookup_count.load(Ordering::Acquire);
        loop {
            let new_count = if lookup_count > old_count {
                tracing::warn!(node_id, "Too many forgets for inode");
                0
            } else {
                old_count - lookup_count
            };

            match self.lookup_count.compare_exchange_weak(
                old_count,
                new_count,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break new_count,
                Err(value) => old_count = value,
            }
        }
    }

    /// Performs a lookup for a child of this inode.
    pub fn lookup_child(&self, name: &LxStr) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = self.child_path(name)?;
        let (inode, stat) = VirtioFsInode::new(Arc::clone(&self.volume), path)?;
        let attr = inode.attr_from_stat(&stat);
        Ok((inode, attr))
    }

    /// Retrieves the attributes of this inode.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.volume.lstat(&*self.get_path())?;
        Ok(self.attr_from_stat(&stat))
    }

    /// Retrieves the extended attributes of this inode.
    pub fn get_statx(&self) -> lx::Result<fuse_statx> {
        let statx = self.volume.statx(&*self.get_path())?;
        Ok(self.statx_from(&statx))
    }

    /// Sets the attributes of this inode.
    pub fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<fuse_attr> {
        let attr = util::fuse_set_attr_to_lxutil(arg, request_uid);

        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared
        // depending on the attributes being set. Lxutil takes care of that on Windows (and Linux
        // does it naturally).
        let stat = self.volume.set_attr_stat(&*self.get_path(), attr)?;
        Ok(self.attr_from_stat(&stat))
    }

    /// Opens the inode, creating a file object.
    pub fn open(self: Arc<VirtioFsInode>, flags: u32) -> lx::Result<VirtioFsFile> {
        let flags = (flags as i32) | lx::O_NOFOLLOW;
        let file = self.volume.open(&*self.get_path(), flags, None)?;
        Ok(VirtioFsFile::new(file, self))
    }

    /// Creates a new file as a child of this inode, and opens it.
    pub fn create(
        &self,
        name: &LxStr,
        flags: u32,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr, lxutil::LxFile)> {
        let path = self.child_path(name)?;
        let options = LxCreateOptions::new(mode, uid, gid);
        let flags = (flags as i32) | lx::O_CREAT | lx::O_NOFOLLOW;
        let file = self.volume.open(&path, flags, Some(options))?;
        let stat = file.fstat()?.into();
        let inode = Self::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = inode.attr_from_stat(&stat);
        Ok((inode, attr, file))
    }

    /// Creates a new directory as a child of this inode.
    pub fn mkdir(
        &self,
        name: &LxStr,
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = self.child_path(name)?;
        let stat = self
            .volume
            .mkdir_stat(&path, LxCreateOptions::new(mode, uid, gid))?;

        let inode = Self::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = inode.attr_from_stat(&stat);
        Ok((inode, attr))
    }

    /// Creates a new regular, device, socket, or fifo file as a child of this inode.
    pub fn mknod(
        &self,
        name: &LxStr,
        mode: u32,
        uid: u32,
        gid: u32,
        device_id: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = self.child_path(name)?;
        let stat = self.volume.mknod_stat(
            &path,
            LxCreateOptions::new(mode, uid, gid),
            device_id as usize,
        )?;

        let inode = Self::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = inode.attr_from_stat(&stat);
        Ok((inode, attr))
    }

    /// Creates a new symlink as a child of this inode.
    pub fn symlink(
        &self,
        name: &LxStr,
        target: &LxStr,
        uid: u32,
        gid: u32,
    ) -> lx::Result<(VirtioFsInode, fuse_attr)> {
        let path = self.child_path(name)?;
        let stat = self.volume.symlink_stat(
            &path,
            target,
            LxCreateOptions::new(lx::S_IFLNK | 0o777, uid, gid),
        )?;

        let inode = Self::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = inode.attr_from_stat(&stat);
        Ok((inode, attr))
    }

    /// Creates a new hard link as a child of this inode.
    pub fn link(&self, name: &LxStr, target: &VirtioFsInode) -> lx::Result<fuse_attr> {
        if self.volume.id() != target.volume.id() {
            return Err(lx::Error::EXDEV);
        }
        let path = self.child_path(name)?;
        let stat = self.volume.link_stat(&*target.get_path(), path)?;
        // The reply describes the shared (target) inode, so namespace via the
        // target rather than this directory inode.
        Ok(target.attr_from_stat(&stat))
    }

    /// Reads the target of the symbolic link, if this inode is a symbolic link.
    pub fn read_link(&self) -> lx::Result<LxString> {
        self.volume.read_link(&*self.get_path())
    }

    /// Removes a file or directory child of this inode.
    pub fn unlink(&self, name: &LxStr, flags: i32) -> lx::Result<()> {
        let path = self.child_path(name)?;
        self.volume.unlink(path, flags)
    }

    /// Renames a child of this inode.
    pub fn rename(
        &self,
        name: &LxStr,
        new_dir: &VirtioFsInode,
        new_name: &LxStr,
        flags: u32,
    ) -> lx::Result<()> {
        let path = self.child_path(name)?;
        let new_path = new_dir.child_path(new_name)?;
        self.volume.rename(path, new_path, flags)
    }

    /// Gets the attributes of the file system that the inode resides on.
    pub fn stat_fs(&self) -> lx::Result<fuse_kstatfs> {
        let stat_fs = self.volume.stat_fs(&*self.get_path())?;
        Ok(fuse_kstatfs::new(
            stat_fs.block_count,
            stat_fs.free_block_count,
            stat_fs.available_block_count,
            stat_fs.file_count,
            stat_fs.available_file_count,
            stat_fs.block_size as u32,
            stat_fs.maximum_file_name_length as u32,
            stat_fs.file_record_size as u32,
        ))
    }

    /// Gets the value or the size of an extended attribute on this inode.
    pub fn get_xattr(&self, name: &LxStr, value: Option<&mut [u8]>) -> lx::Result<usize> {
        self.volume.get_xattr(&*self.get_path(), name, value)
    }

    /// Sets an extended attribute on this inode.
    pub fn set_xattr(&self, name: &LxStr, value: &[u8], flags: u32) -> lx::Result<()> {
        self.volume
            .set_xattr(&*self.get_path(), name, value, flags as i32)
    }

    /// Lists the extended attributes on this inode.
    pub fn list_xattr(&self, list: Option<&mut [u8]>) -> lx::Result<usize> {
        self.volume.list_xattr(&*self.get_path(), list)
    }

    /// Removes an extended attribute from this inode.
    pub fn remove_xattr(&self, name: &LxStr) -> lx::Result<()> {
        self.volume.remove_xattr(&*self.get_path(), name)
    }

    /// Gets a clone of the stored path.
    pub fn clone_path(&self) -> PathBuf {
        self.get_path().clone()
    }

    /// The key used to deduplicate this inode in the `InodeMap`, so that
    /// repeated lookups of the same host file return one stable FUSE node id.
    ///
    /// Stable-id volumes key by `(volume_id, inode_nr)`. Volumes that recycle
    /// inode numbers (FAT/exFAT) key by `(volume_id, path)` instead, so a given
    /// path maps to a single, stable FUSE node id across lookups. This includes
    /// the empty path of a volume root, so an aggregate child root keeps one
    /// stable node id across repeated lookups rather than allocating a fresh one
    /// each time.
    pub(crate) fn dedup_key(&self) -> DedupKey {
        if self.volume.supports_stable_file_id() {
            return DedupKey::Ino(self.volume_id(), self.inode_nr());
        }
        DedupKey::Path(self.volume_id(), self.get_path().clone())
    }

    /// The [`DedupKey::Path`] that a child named `name` of this inode would use,
    /// for path-keyed (non-stable-id) volumes only.
    pub(crate) fn child_path_dedup_key(&self, name: &LxStr) -> Option<DedupKey> {
        if self.volume.supports_stable_file_id() {
            return None;
        }
        let path = self.child_path(name).ok()?;
        Some(DedupKey::Path(self.volume_id(), path))
    }

    /// Appends a child name to this inode's path.
    fn child_path(&self, name: &LxStr) -> lx::Result<PathBuf> {
        // Defense in depth: the FUSE request parser already validates names,
        // but assert here to catch any bypass.
        assert!(!name.is_empty(), "empty child name");
        assert!(!name.as_bytes().contains(&b'/'), "child name contains '/'");
        assert!(name != "." && name != "..", "child name is '.' or '..'");

        let mut path = self.clone_path();
        path.push_lx(name)?;
        Ok(path)
    }

    /// Locks the path and returns the value.
    fn get_path(&self) -> parking_lot::RwLockReadGuard<'_, PathBuf> {
        self.path.read()
    }
}
