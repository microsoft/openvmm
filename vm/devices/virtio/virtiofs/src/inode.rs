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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

/// Implements inode callbacks for virtio-fs.
pub struct VirtioFsInode {
    volume: Arc<LxVolume>,
    path: RwLock<PathBuf>,
    lookup_count: AtomicU64,
    inode_nr: lx::ino_t,
}

impl VirtioFsInode {
    /// Create a new inode for the specified path.
    pub fn new(volume: Arc<LxVolume>, path: PathBuf) -> lx::Result<(Self, lx::Stat)> {
        let stat = volume.lstat(&path)?;
        let inode = Self::with_attr(volume, path, &stat);
        Ok((inode, stat))
    }

    /// Create a new inode for the specified path, with previously retrieved attributes.
    pub fn with_attr(volume: Arc<LxVolume>, path: PathBuf, stat: &lx::Stat) -> Self {
        Self {
            volume,
            path: RwLock::new(path),
            lookup_count: AtomicU64::new(1),
            inode_nr: stat.inode_nr,
        }
    }

    /// Return the files inode number as reported by the underlying file system.
    ///
    /// N.B. This may be different from its FUSE node ID.
    pub fn inode_nr(&self) -> lx::ino_t {
        self.inode_nr
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
        let attr = util::stat_to_fuse_attr(&stat);
        Ok((inode, attr))
    }

    /// Retrieves the attributes of this inode.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.volume.lstat(&*self.get_path())?;
        Ok(util::stat_to_fuse_attr(&stat))
    }

    /// Sets the attributes of this inode.
    pub fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<fuse_attr> {
        let attr = util::fuse_set_attr_to_lxutil(arg, request_uid);

        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared
        // depending on the attributes being set. Lxutil takes care of that on Windows (and Linux
        // does it naturally).
        let stat = self.volume.set_attr_stat(&*self.get_path(), attr)?;
        Ok(util::stat_to_fuse_attr(&stat))
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
        let stat = file.fstat()?;
        let inode = Self::with_attr(Arc::clone(&self.volume), path, &stat);
        let attr = util::stat_to_fuse_attr(&stat);
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
        let attr = util::stat_to_fuse_attr(&stat);
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
        let attr = util::stat_to_fuse_attr(&stat);
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
        let attr = util::stat_to_fuse_attr(&stat);
        Ok((inode, attr))
    }

    /// Creates a new hard link as a child of this inode.
    pub fn link(&self, name: &LxStr, target: &VirtioFsInode) -> lx::Result<fuse_attr> {
        let path = self.child_path(name)?;
        let stat = self.volume.link_stat(&*target.get_path(), path)?;
        Ok(util::stat_to_fuse_attr(&stat))
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

    /// Appends a child name to this inode's path.
    fn child_path(&self, name: &LxStr) -> lx::Result<PathBuf> {
        let mut path = self.clone_path();
        path.push_lx(name)?;
        Ok(path)
    }

    /// Locks the path and returns the value.
    fn get_path(&self) -> parking_lot::RwLockReadGuard<'_, PathBuf> {
        self.path.read()
    }
}
