// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(any(windows, target_os = "linux"))]

mod aggregate;
mod file;
mod inode;
#[cfg(test)]
mod integration_tests;
pub mod resolver;
#[cfg(windows)]
mod section;
mod util;
pub mod virtio;
mod virtio_util;

#[cfg(windows)]
pub use section::SectionFs;

use aggregate::AggregateState;
use aggregate::SYNTHETIC_ROOT_FH;
use file::VirtioFsFile;
use fuse::protocol::*;
use fuse::*;
use inode::DedupKey;
use inode::VirtioFsInode;
use inode::VirtioFsVolume;
pub use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// TODO: Make these configurable.
// FUSE likes to spam getattr a lot, so having a small timeout on the attributes avoids excessive
// calls. It also means that a lookup/stat sequence can use the attributes returned by lookup
// rather than having to call getattr.
const ATTRIBUTE_TIMEOUT: Duration = Duration::from_millis(1);

// Entry timeout must be zero, because on rename existing entries for the child being renamed do
// not get updated and would stop working. Having a zero timeout forces a new lookup which will
// update the path.
const ENTRY_TIMEOUT: Duration = Duration::from_secs(0);

/// Shared mutable state behind a [`VirtioFs`] handle.
struct VirtioFsInner {
    inodes: RwLock<InodeMap>,
    files: RwLock<HandleMap<Arc<VirtioFsFile>>>,
    mode: VirtioFsMode,
}

/// Distinguishes a single-share device from a multi-share aggregate.
///
/// The read-only setting lives on each volume's inodes, not here, so aggregate
/// children can differ (see [`AggregateState`]).
enum VirtioFsMode {
    /// Single share: node 1 is a real inode at the volume root.
    Direct,
    /// Multi-share: node 1 is a synthetic directory whose children are
    /// independent host folders.
    Aggregate(AggregateState),
}

impl VirtioFsInner {
    /// The aggregate state, or `None` for a direct (single-share) device.
    fn aggregate(&self) -> Option<&AggregateState> {
        match &self.mode {
            VirtioFsMode::Aggregate(state) => Some(state),
            VirtioFsMode::Direct => None,
        }
    }
}

fn build_volume(
    root_path: impl AsRef<Path>,
    mount_options: Option<&LxVolumeOptions>,
) -> lx::Result<(lxutil::LxVolume, bool)> {
    let readonly = mount_options.is_some_and(|options| options.is_readonly());
    let volume = if let Some(mount_options) = mount_options {
        mount_options.new_volume(root_path)
    } else {
        lxutil::LxVolume::new(root_path)
    }?;
    Ok((volume, readonly))
}

/// Implementation of the virtio-fs file system.
#[derive(Clone)]
pub struct VirtioFs {
    inner: Arc<VirtioFsInner>,
}

impl Fuse for VirtioFs {
    fn init(&self, info: &mut SessionInfo) {
        // Indicate we support both readdir and readdirplus.
        if info.capable() & FUSE_DO_READDIRPLUS != 0 {
            info.want |= FUSE_DO_READDIRPLUS;
        }

        // Using "auto" lets FUSE pick whether to use readdir or readdirplus, which can be
        // beneficial since readdirplus needs to query every file and is therefore more expensive.
        if info.capable() & FUSE_READDIRPLUS_AUTO != 0 {
            info.want |= FUSE_READDIRPLUS_AUTO;
        }

        // Allow shared mmap on files opened with FOPEN_DIRECT_IO. This is
        // relevant for virtiofs where direct-I/O is used to avoid page-cache
        // coherency issues with the host, but applications still need mmap.
        if info.capable2() & FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2 != 0 {
            info.want2 |= FUSE_DIRECT_IO_ALLOW_MMAP_FLAG2;
        }
    }

    fn get_attr(&self, request: &Request, flags: u32, fh: u64) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted. The synthetic root's directory handle has no
        // backing file, so fall through to the node-based branch for it.
        let attr = if flags & FUSE_GETATTR_FH != 0 && !self.is_synthetic_root_handle(node_id, fh) {
            let file = self.get_file(fh)?;
            file.get_attr()?
        } else if self.is_synthetic_root(node_id) {
            self.synthetic_root_attr()
        } else {
            let inode = self.get_inode(node_id)?;
            inode.get_attr()?
        };

        Ok(fuse_attr_out::new(ATTRIBUTE_TIMEOUT, attr))
    }

    fn get_statx(
        &self,
        request: &Request,
        fh: u64,
        getattr_flags: u32,
        flags: StatxFlags,
        mask: lx::StatExMask,
    ) -> lx::Result<fuse_statx_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted. The synthetic root's directory handle has no
        // backing file, so fall through to the node-based branch for it.
        let statx = if getattr_flags & FUSE_GETATTR_FH != 0
            && !self.is_synthetic_root_handle(node_id, fh)
        {
            let file = self.get_file(fh)?;
            file.get_statx()?
        } else if self.is_synthetic_root(node_id) {
            self.synthetic_root_statx(mask)
        } else {
            let inode = self.get_inode(node_id)?;
            inode.get_statx()?
        };

        Ok(fuse_statx_out::new(ATTRIBUTE_TIMEOUT, flags, statx))
    }

    fn set_attr(&self, request: &Request, arg: &fuse_setattr_in) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();

        if self.is_synthetic_root(node_id) {
            return Err(lx::Error::EROFS);
        }

        // If a file handle is specified, set the attributes on the open file. This is faster on
        // Windows and works if the file was deleted.
        let attr = if arg.valid & FATTR_FH != 0 {
            let file = self.get_file(arg.fh)?;
            // Block truncation and other modifications on readonly filesystems
            if arg.valid & !(FATTR_FH | FATTR_LOCKOWNER) != 0 {
                self.check_writable(file.inode())?;
            }
            file.set_attr(arg, request.uid())?;
            file.get_attr()?
        } else {
            let inode = self.get_inode(node_id)?;
            // Block truncation and other modifications on readonly filesystems
            if arg.valid & !(FATTR_FH | FATTR_LOCKOWNER) != 0 {
                self.check_writable(&inode)?;
            }
            inode.set_attr(arg, request.uid())?
        };

        Ok(fuse_attr_out::new(ATTRIBUTE_TIMEOUT, attr))
    }

    fn lookup(&self, request: &Request, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return self.lookup_synthetic_root(name);
        }
        let inode = self.get_inode(request.node_id())?;
        self.lookup_helper(&inode, name)
    }

    fn forget(&self, node_id: u64, lookup_count: u64) {
        // This must be done under lock so an inode can't be resurrected between the lookup count
        // reaching zero and removing it from the list.
        let mut inodes = self.inner.inodes.write();
        if let Some(inode) = inodes.get(node_id) {
            if inode.forget(node_id, lookup_count) == 0 {
                tracing::trace!(node_id, "Removing inode");
                inodes.remove(node_id);
            }
        }
    }

    fn open(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        let inode = self.get_inode(request.node_id())?;
        self.check_open_readonly(&inode, flags)?;
        let file = inode.open(flags)?;
        let fh = self.insert_file(file);

        // TODO: Optionally allow caching.
        Ok(fuse_open_out::new(fh, FOPEN_DIRECT_IO))
    }

    fn create(
        &self,
        request: &Request,
        name: &lx::LxStr,
        arg: &fuse_create_in,
    ) -> lx::Result<CreateOut> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let (new_inode, attr, file) =
            inode.create(name, arg.flags, arg.mode, request.uid(), request.gid())?;

        // Insert the newly created inode; this can return an existing inode if it found a match
        // on the inode number (if this is a non-exclusive create), so make sure to associate the
        // file with the returned inode.
        let (new_inode, node_id) = self.insert_inode(new_inode);
        let file = VirtioFsFile::new(file, new_inode);
        let fh = self.insert_file(file);
        Ok(CreateOut {
            entry: fuse_entry_out::new(node_id, ENTRY_TIMEOUT, ATTRIBUTE_TIMEOUT, attr),
            open: fuse_open_out::new(fh, FOPEN_DIRECT_IO),
        })
    }

    fn mkdir(
        &self,
        request: &Request,
        name: &lx::LxStr,
        arg: &fuse_mkdir_in,
    ) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let (new_inode, attr) = inode.mkdir(name, arg.mode, request.uid(), request.gid())?;
        let (_, node_id) = self.insert_inode(new_inode);
        Ok(fuse_entry_out::new(
            node_id,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    fn mknod(
        &self,
        request: &Request,
        name: &lx::LxStr,
        arg: &fuse_mknod_in,
    ) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let (new_inode, attr) =
            inode.mknod(name, arg.mode, request.uid(), request.gid(), arg.rdev)?;

        let (_, node_id) = self.insert_inode(new_inode);
        Ok(fuse_entry_out::new(
            node_id,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    fn symlink(
        &self,
        request: &Request,
        name: &lx::LxStr,
        target: &lx::LxStr,
    ) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        let (new_inode, attr) = inode.symlink(name, target, request.uid(), request.gid())?;

        let (_, node_id) = self.insert_inode(new_inode);
        Ok(fuse_entry_out::new(
            node_id,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    fn link(&self, request: &Request, name: &lx::LxStr, target: u64) -> lx::Result<fuse_entry_out> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        let target_inode = self.get_inode(target)?;
        self.check_writable(&inode)?;
        let attr = inode.link(name, &target_inode)?;

        // Increment the lookup count since we're returning an entry for this inode.
        // The kernel will send a forget for this entry later.
        target_inode.inc_lookup();

        // Use the target inode as the reply, with refreshed attributes.
        Ok(fuse_entry_out::new(
            target,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    fn read_link(&self, request: &Request) -> lx::Result<lx::LxString> {
        let inode = self.get_inode(request.node_id())?;
        inode.read_link()
    }

    fn read(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        let file = self.get_file(arg.fh)?;
        let mut buffer = vec![0u8; arg.size as usize];
        let size = file.read(&mut buffer, arg.offset)?;
        buffer.truncate(size);
        Ok(buffer)
    }

    fn write(&self, request: &Request, arg: &fuse_write_in, data: &[u8]) -> lx::Result<usize> {
        let file = self.get_file(arg.fh)?;
        self.check_writable(file.inode())?;
        file.write(data, arg.offset, request.uid())
    }

    fn release(&self, _request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
        self.remove_file(arg.fh);
        Ok(())
    }

    fn open_dir(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        if self.is_synthetic_root(request.node_id()) {
            // The synthetic root has no backing handle; hand out a sentinel that
            // read_dir/read_dir_plus/release_dir recognize.
            return Ok(fuse_open_out::new(SYNTHETIC_ROOT_FH, 0));
        }
        // There is no special handling for directories, so just call open.
        self.open(request, flags)
    }

    fn read_dir(&self, request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root_handle(request.node_id(), arg.fh) {
            return self.read_synthetic_root_dir(arg.offset, arg.size, false);
        }
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, false)
    }

    fn read_dir_plus(&self, request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root_handle(request.node_id(), arg.fh) {
            return self.read_synthetic_root_dir(arg.offset, arg.size, true);
        }
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, true)
    }

    fn release_dir(&self, request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
        if self.is_synthetic_root_handle(request.node_id(), arg.fh) {
            return Ok(());
        }
        self.release(request, arg)
    }

    fn unlink(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        self.unlink_helper(request, name, 0)
    }

    fn rmdir(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        self.unlink_helper(request, name, lx::AT_REMOVEDIR)
    }

    fn rename(
        &self,
        request: &Request,
        name: &lx::LxStr,
        new_dir: u64,
        new_name: &lx::LxStr,
        flags: u32,
    ) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) || self.is_synthetic_root(new_dir) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        let new_inode = self.get_inode(new_dir)?;
        // A rename cannot cross aggregated volume boundaries.
        if inode.volume_id() != new_inode.volume_id() {
            return Err(lx::Error::EXDEV);
        }
        self.check_writable(&inode)?;
        inode.rename(name, &new_inode, new_name, flags)?;
        // A rename doesn't preserve inode identity on path-keyed volumes, so
        // evict both the vacated source path and the overwritten destination
        // path from the dedup map.
        let mut inodes = self.inner.inodes.write();
        if let Some(key) = inode.child_path_dedup_key(name) {
            inodes.evict_dedup_key(&key);
        }
        if let Some(key) = new_inode.child_path_dedup_key(new_name) {
            inodes.evict_dedup_key(&key);
        }
        Ok(())
    }

    fn statfs(&self, request: &Request) -> lx::Result<fuse_kstatfs> {
        if self.is_synthetic_root(request.node_id()) {
            return Ok(fuse_kstatfs::new(0, 0, 0, 0, 0, 512, 255, 512));
        }
        let inode = self.get_inode(request.node_id())?;
        inode.stat_fs()
    }

    fn fsync(&self, _request: &Request, fh: u64, flags: u32) -> lx::Result<()> {
        let file = self.get_file(fh)?;
        let data_only = flags & FUSE_FSYNC_FDATASYNC != 0;
        file.fsync(data_only)
    }

    fn fsync_dir(&self, request: &Request, fh: u64, flags: u32) -> lx::Result<()> {
        self.fsync(request, fh, flags)
    }

    fn get_xattr(&self, request: &Request, name: &lx::LxStr, size: u32) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::ENODATA);
        }
        let inode = self.get_inode(request.node_id())?;
        let mut value = vec![0u8; size as usize];
        let size = inode.get_xattr(name, Some(&mut value))?;
        value.truncate(size);
        Ok(value)
    }

    fn get_xattr_size(&self, request: &Request, name: &lx::LxStr) -> lx::Result<u32> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::ENODATA);
        }
        let inode = self.get_inode(request.node_id())?;
        let size = inode.get_xattr(name, None)?;
        let size = size.try_into().map_err(|_| lx::Error::E2BIG)?;
        Ok(size)
    }

    fn set_xattr(
        &self,
        request: &Request,
        name: &lx::LxStr,
        value: &[u8],
        flags: u32,
    ) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        inode.set_xattr(name, value, flags)
    }

    fn list_xattr(&self, request: &Request, size: u32) -> lx::Result<Vec<u8>> {
        if self.is_synthetic_root(request.node_id()) {
            return Ok(Vec::new());
        }
        let inode = self.get_inode(request.node_id())?;
        let mut list = vec![0u8; size as usize];
        let size = inode.list_xattr(Some(&mut list))?;
        list.truncate(size);
        Ok(list)
    }

    fn list_xattr_size(&self, request: &Request) -> lx::Result<u32> {
        if self.is_synthetic_root(request.node_id()) {
            return Ok(0);
        }
        let inode = self.get_inode(request.node_id())?;
        let size = inode.list_xattr(None)?;
        let size = size.try_into().map_err(|_| lx::Error::E2BIG)?;
        Ok(size)
    }

    fn remove_xattr(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        inode.remove_xattr(name)
    }

    fn destroy(&self) {
        // To get the file system ready for re-mount, clean out any open files and leaked inodes.
        self.inner.files.write().clear();
        self.inner.inodes.write().clear();
    }
}

impl VirtioFs {
    /// Check if the inode's volume is readonly and return EROFS if so.
    fn check_writable(&self, inode: &VirtioFsInode) -> lx::Result<()> {
        if inode.readonly() {
            Err(lx::Error::EROFS)
        } else {
            Ok(())
        }
    }

    /// Check whether the open flags are permitted on a read-only filesystem.
    fn check_open_readonly(&self, inode: &VirtioFsInode, flags: u32) -> lx::Result<()> {
        if !inode.readonly() {
            return Ok(());
        }

        // This section exists to superceed error codes when various combination of flags
        // are passed to the open() call. This helps maintain POSIX compatibility
        // If O_CREAT | O_EXCL && file_exists => EEXIST
        // If O_CREAT && file_exists => fallthrough to check other checks
        // If O_CREAT && !file_exists => EROFS
        // Other errors that occur while checking file_exists should bubble up
        if flags & lx::O_CREAT as u32 != 0 {
            match inode.get_attr() {
                Ok(_) if flags & lx::O_EXCL as u32 != 0 => return Err(lx::Error::EEXIST),
                Ok(_) => {}
                Err(e) if e == lx::Error::ENOENT => return Err(lx::Error::EROFS),
                Err(e) => return Err(e),
            }
        } else {
            inode.get_attr()?;
        }

        let access_mode = (flags & lx::O_ACCESS_MASK as u32) as i32;
        if matches!(access_mode, lx::O_WRONLY | lx::O_RDWR) || flags & lx::O_TRUNC as u32 != 0 {
            return Err(lx::Error::EROFS);
        }

        Ok(())
    }

    /// Create a new virtio-fs for the specified root path.
    pub fn new(
        root_path: impl AsRef<Path>,
        mount_options: Option<&LxVolumeOptions>,
    ) -> lx::Result<Self> {
        let (volume, readonly) = build_volume(root_path, mount_options)?;
        let mut inodes = InodeMap::new(false);
        let volume = Arc::new(VirtioFsVolume::new(volume, 0, readonly));
        let (root_inode, _) = VirtioFsInode::new(volume, PathBuf::new())?;
        assert!(inodes.insert(root_inode).1 == FUSE_ROOT_ID);
        Ok(Self {
            inner: Arc::new(VirtioFsInner {
                inodes: RwLock::new(inodes),
                files: RwLock::new(HandleMap::new()),
                mode: VirtioFsMode::Direct,
            }),
        })
    }

    /// Create a new, empty aggregate virtio-fs.
    ///
    /// Node 1 is a synthetic, read-only directory; use [`Self::add_child`] to
    /// expose host folders as named children, each with its own read-only
    /// setting. Children share one superblock, with inode numbers namespaced
    /// per volume to avoid cross-volume `st_ino` collisions.
    pub fn new_aggregate() -> Self {
        Self {
            inner: Arc::new(VirtioFsInner {
                // `true` enables aggregate mode: node 1 is synthetic (see `InodeMap`).
                inodes: RwLock::new(InodeMap::new(true)),
                files: RwLock::new(HandleMap::new()),
                mode: VirtioFsMode::Aggregate(AggregateState::new()),
            }),
        }
    }

    fn lookup_helper(&self, inode: &VirtioFsInode, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        let (new_inode, attr) = inode.lookup_child(name)?;
        let (_, new_inode_nr) = self.insert_inode(new_inode);
        Ok(fuse_entry_out::new(
            new_inode_nr,
            ENTRY_TIMEOUT,
            ATTRIBUTE_TIMEOUT,
            attr,
        ))
    }

    /// Removes a file or directory.
    fn unlink_helper(&self, request: &Request, name: &lx::LxStr, flags: i32) -> lx::Result<()> {
        if self.is_synthetic_root(request.node_id()) {
            return Err(lx::Error::EROFS);
        }
        let inode = self.get_inode(request.node_id())?;
        self.check_writable(&inode)?;
        inode.unlink(name, flags)?;
        // On path-keyed volumes the path is the inode's identity, so evict it
        // now; a later create at the same path must not alias the removed inode.
        if let Some(key) = inode.child_path_dedup_key(name) {
            self.inner.inodes.write().evict_dedup_key(&key);
        }
        Ok(())
    }

    /// Retrieve the inode with the specified node ID.
    fn get_inode(&self, node_id: u64) -> lx::Result<Arc<VirtioFsInode>> {
        self.inner.inodes.read().get(node_id).ok_or_else(|| {
            tracing::warn!(node_id, "request for unknown inode");
            lx::Error::EINVAL
        })
    }

    /// Insert a new inode, and returns the assigned node ID as well as a reference to the inode.
    ///
    /// If the file system supports stable inode numbers and an inode already existed with this
    /// number, the existing inode is returned, not the passed in one.
    fn insert_inode(&self, inode: VirtioFsInode) -> (Arc<VirtioFsInode>, u64) {
        self.inner.inodes.write().insert(inode)
    }

    /// Retrieve the file object with the specified file handle.
    fn get_file(&self, fh: u64) -> lx::Result<Arc<VirtioFsFile>> {
        let files = self.inner.files.read();
        let file = files.get(fh).ok_or_else(|| {
            tracing::warn!(fh, "Request for unknown file");
            lx::Error::EBADF
        })?;

        Ok(Arc::clone(file))
    }

    /// Insert a new file object, and return the assigned file handle.
    fn insert_file(&self, file: VirtioFsFile) -> u64 {
        self.inner.files.write().insert(Arc::new(file))
    }

    /// Remove the file with the specified node ID.
    fn remove_file(&self, fh: u64) {
        self.inner.files.write().remove(fh);
    }
}

/// A key/value map where the keys are automatically incremented identifiers.
struct HandleMap<T> {
    values: HashMap<u64, T>,
    next_handle: u64,
}

impl<T> HandleMap<T> {
    /// Create a new `HandleMap`.
    pub fn new() -> Self {
        Self::starting_at(1)
    }

    /// Create a new `HandleMap` starting with handle value `next_handle`.
    pub fn starting_at(next_handle: u64) -> Self {
        Self {
            values: HashMap::new(),
            next_handle,
        }
    }

    /// Inserts an item into the map, and returns the assigned handle.
    pub fn insert(&mut self, value: T) -> u64 {
        let handle = self.next_handle;
        if self.values.insert(handle, value).is_some() {
            panic!("Inode number reused.");
        }

        self.next_handle += 1;
        handle
    }

    /// Retrieves a value from the map.
    pub fn get(&self, handle: u64) -> Option<&T> {
        self.values.get(&handle)
    }

    /// Retrieves a value from the map.
    #[cfg_attr(not(windows), expect(dead_code))]
    pub fn get_mut(&mut self, handle: u64) -> Option<&mut T> {
        self.values.get_mut(&handle)
    }

    /// Removes a value from the map.
    pub fn remove(&mut self, handle: u64) -> Option<T> {
        self.values.remove(&handle)
    }

    /// Clears the map and resets the handle values.
    pub fn clear(&mut self) {
        self.values.clear();
        self.next_handle = 1;
    }
}

/// Assigns node IDs to inodes, and keeps track of in-use inodes by their actual inode number.
///
/// We cannot use the real inode number as the FUSE node ID:
/// - FUSE node ID 1 is reserved for the root, so this would break if a file system used that inode
///   number.
/// - When we want to support multiple volumes in a single file system, node IDs still need to be
///   globally unique, whereas inode numbers are per-volume.
struct InodeMap {
    inodes_by_node_id: HandleMap<Arc<VirtioFsInode>>,
    /// Maps a [`DedupKey`] to the registered inode and its FUSE node id, so
    /// repeated lookups of one host file share a single node id.
    inodes_by_key: HashMap<DedupKey, (Arc<VirtioFsInode>, u64)>,
    /// When true, node 1 is synthetic and not stored in this map, so node IDs
    /// are allocated starting at 2 and `clear` does not preserve a real root.
    aggregate: bool,
}

impl InodeMap {
    /// Create a new `InodeMap`.
    pub fn new(aggregate: bool) -> Self {
        Self {
            inodes_by_node_id: if aggregate {
                HandleMap::starting_at(FUSE_ROOT_ID + 1)
            } else {
                HandleMap::new()
            },
            inodes_by_key: HashMap::new(),
            aggregate,
        }
    }

    /// Get an inode with the specified FUSE node ID.
    pub fn get(&self, node_id: u64) -> Option<Arc<VirtioFsInode>> {
        let inode = self.inodes_by_node_id.get(node_id)?;
        Some(Arc::clone(inode))
    }

    /// Insert an inode into the map, returning its node ID.
    pub fn insert(&mut self, inode: VirtioFsInode) -> (Arc<VirtioFsInode>, u64) {
        // Reuse an existing node id for the same host file; see `DedupKey`
        // for how each volume type is keyed.
        match self.inodes_by_key.entry(inode.dedup_key()) {
            Entry::Occupied(entry) => {
                // Inode found; increment its count and return the existing FUSE node ID.
                let new_path = inode.clone_path();
                let (existing, node_id) = entry.get();
                existing.lookup(new_path);
                (Arc::clone(existing), *node_id)
            }
            Entry::Vacant(entry) => {
                // Inode not found, so insert it into both maps.
                let inode = Arc::new(inode);
                let node_id = self.inodes_by_node_id.insert(Arc::clone(&inode));
                entry.insert((Arc::clone(&inode), node_id));
                (inode, node_id)
            }
        }
    }

    /// Remove an inode with the specified FUSE node ID from the map.
    pub fn remove(&mut self, node_id: u64) {
        let inode = self.inodes_by_node_id.remove(node_id).unwrap();
        // Only drop the by-key entry if it still points at THIS node: on
        // path-keyed volumes the path may have been repointed to a newer inode
        // (via delete+recreate or `evict_dedup_key`), which must not be lost.
        if let Entry::Occupied(entry) = self.inodes_by_key.entry(inode.dedup_key()) {
            if entry.get().1 == node_id {
                entry.remove();
            }
        }
    }

    /// Detach a [`DedupKey::Path`] entry from its current inode so a later
    /// create at that path gets a fresh node id instead of aliasing the
    /// removed/renamed file. The inode stays in `inodes_by_node_id` for any
    /// live fd or watch.
    pub fn evict_dedup_key(&mut self, key: &DedupKey) {
        if matches!(key, DedupKey::Path(..)) {
            self.inodes_by_key.remove(key);
        }
    }

    /// Clears the map, preserving the root inode.
    pub fn clear(&mut self) {
        if self.aggregate {
            // Node 1 is synthetic and not stored here; drop everything and resume
            // allocating node IDs after the reserved root id.
            self.inodes_by_node_id.clear();
            self.inodes_by_node_id.next_handle = FUSE_ROOT_ID + 1;
            self.inodes_by_key.clear();
            return;
        }

        let root_inode = Arc::clone(self.inodes_by_node_id.get(FUSE_ROOT_ID).unwrap());
        self.inodes_by_node_id.clear();

        // Re-insert the root inode.
        assert!(self.inodes_by_node_id.insert(Arc::clone(&root_inode)) == FUSE_ROOT_ID);

        // Rebuild the dedup map with just the root.
        self.inodes_by_key.clear();
        let key = root_inode.dedup_key();
        self.inodes_by_key.insert(key, (root_inode, FUSE_ROOT_ID));
    }
}
