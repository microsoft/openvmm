// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(any(windows, target_os = "linux"))]

mod file;
mod inode;
pub mod resolver;
#[cfg(windows)]
mod section;
mod util;
pub mod virtio;
mod virtio_util;

#[cfg(windows)]
pub use section::SectionFs;

use file::VirtioFsFile;
use fuse::protocol::*;
use fuse::*;
use inode::VirtioFsInode;
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

/// Implementation of the virtio-fs file system.
pub struct VirtioFs {
    inodes: RwLock<InodeMap>,
    files: RwLock<HandleMap<Arc<VirtioFsFile>>>,
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
    }

    fn get_attr(&self, request: &Request, flags: u32, fh: u64) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted.
        let attr = if flags & FUSE_GETATTR_FH != 0 {
            let file = self.get_file(fh)?;
            file.get_attr()?
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
        _mask: lx::StatExMask,
    ) -> lx::Result<fuse_statx_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted.
        let statx = if getattr_flags & FUSE_GETATTR_FH != 0 {
            let file = self.get_file(fh)?;
            file.get_statx()?
        } else {
            let inode = self.get_inode(node_id)?;
            inode.get_statx()?
        };

        Ok(fuse_statx_out::new(ATTRIBUTE_TIMEOUT, flags, statx))
    }

    fn set_attr(&self, request: &Request, arg: &fuse_setattr_in) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();

        // If a file handle is specified, set the attributes on the open file. This is faster on
        // Windows and works if the file was deleted.
        let attr = if arg.valid & FATTR_FH != 0 {
            let file = self.get_file(arg.fh)?;
            file.set_attr(arg, request.uid())?;
            file.get_attr()?
        } else {
            let inode = self.get_inode(node_id)?;
            inode.set_attr(arg, request.uid())?
        };

        Ok(fuse_attr_out::new(ATTRIBUTE_TIMEOUT, attr))
    }

    fn lookup(&self, request: &Request, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        let inode = self.get_inode(request.node_id())?;
        self.lookup_helper(&inode, name)
    }

    fn forget(&self, node_id: u64, lookup_count: u64) {
        // This must be done under lock so an inode can't be resurrected between the lookup count
        // reaching zero and removing it from the list.
        let mut inodes = self.inodes.write();
        if let Some(inode) = inodes.get(node_id) {
            if inode.forget(node_id, lookup_count) == 0 {
                tracing::trace!(node_id, "Removing inode");
                inodes.remove(node_id);
            }
        }
    }

    fn open(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        let inode = self.get_inode(request.node_id())?;
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
        let inode = self.get_inode(request.node_id())?;
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
        let inode = self.get_inode(request.node_id())?;
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
        let inode = self.get_inode(request.node_id())?;
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
        let inode = self.get_inode(request.node_id())?;
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
        let inode = self.get_inode(request.node_id())?;
        let target_inode = self.get_inode(target)?;
        let attr = inode.link(name, &target_inode)?;

        // Increment the lookup count since returning fuse_entry_out creates a
        // new kernel reference that will be released via forget().
        target_inode.add_ref();

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
        file.write(data, arg.offset, request.uid())
    }

    fn release(&self, _request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
        self.remove_file(arg.fh);
        Ok(())
    }

    fn open_dir(&self, request: &Request, flags: u32) -> lx::Result<fuse_open_out> {
        // There is no special handling for directories, so just call open.
        self.open(request, flags)
    }

    fn read_dir(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, false)
    }

    fn read_dir_plus(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, true)
    }

    fn release_dir(&self, request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
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
        let inode = self.get_inode(request.node_id())?;
        let new_inode = self.get_inode(new_dir)?;
        inode.rename(name, &new_inode, new_name, flags)
    }

    fn statfs(&self, request: &Request) -> lx::Result<fuse_kstatfs> {
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
        let inode = self.get_inode(request.node_id())?;
        let mut value = vec![0u8; size as usize];
        let size = inode.get_xattr(name, Some(&mut value))?;
        value.truncate(size);
        Ok(value)
    }

    fn get_xattr_size(&self, request: &Request, name: &lx::LxStr) -> lx::Result<u32> {
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
        let inode = self.get_inode(request.node_id())?;
        inode.set_xattr(name, value, flags)
    }

    fn list_xattr(&self, request: &Request, size: u32) -> lx::Result<Vec<u8>> {
        let inode = self.get_inode(request.node_id())?;
        let mut list = vec![0u8; size as usize];
        let size = inode.list_xattr(Some(&mut list))?;
        list.truncate(size);
        Ok(list)
    }

    fn list_xattr_size(&self, request: &Request) -> lx::Result<u32> {
        let inode = self.get_inode(request.node_id())?;
        let size = inode.list_xattr(None)?;
        let size = size.try_into().map_err(|_| lx::Error::E2BIG)?;
        Ok(size)
    }

    fn remove_xattr(&self, request: &Request, name: &lx::LxStr) -> lx::Result<()> {
        let inode = self.get_inode(request.node_id())?;
        inode.remove_xattr(name)
    }

    fn destroy(&self) {
        // To get the file system ready for re-mount, clean out any open files and leaked inodes.
        self.files.write().clear();
        self.inodes.write().clear();
    }
}

impl VirtioFs {
    /// Create a new virtio-fs for the specified root path.
    pub fn new(
        root_path: impl AsRef<Path>,
        mount_options: Option<&LxVolumeOptions>,
    ) -> lx::Result<Self> {
        let volume = if let Some(mount_options) = mount_options {
            mount_options.new_volume(root_path)
        } else {
            lxutil::LxVolume::new(root_path)
        }?;
        let mut inodes = InodeMap::new(volume.supports_stable_file_id());
        let (root_inode, _) = VirtioFsInode::new(Arc::new(volume), PathBuf::new())?;
        assert!(inodes.insert(root_inode).1 == FUSE_ROOT_ID);
        Ok(Self {
            inodes: RwLock::new(inodes),
            files: RwLock::new(HandleMap::new()),
        })
    }

    /// Perform lookup on a specified directory inode.
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
        let inode = self.get_inode(request.node_id())?;
        inode.unlink(name, flags)
    }

    /// Retrieve the inode with the specified node ID.
    fn get_inode(&self, node_id: u64) -> lx::Result<Arc<VirtioFsInode>> {
        self.inodes.read().get(node_id).ok_or_else(|| {
            tracing::warn!(node_id, "request for unknown inode");
            lx::Error::EINVAL
        })
    }

    /// Insert a new inode, and returns the assigned node ID as well as a reference to the inode.
    ///
    /// If the file system supports stable inode numbers and an inode already existed with this
    /// number, the existing inode is returned, not the passed in one.
    fn insert_inode(&self, inode: VirtioFsInode) -> (Arc<VirtioFsInode>, u64) {
        self.inodes.write().insert(inode)
    }

    /// Retrieve the file object with the specified file handle.
    fn get_file(&self, fh: u64) -> lx::Result<Arc<VirtioFsFile>> {
        let files = self.files.read();
        let file = files.get(fh).ok_or_else(|| {
            tracing::warn!(fh, "Request for unknown file");
            lx::Error::EBADF
        })?;

        Ok(Arc::clone(file))
    }

    /// Insert a new file object, and return the assigned file handle.
    fn insert_file(&self, file: VirtioFsFile) -> u64 {
        self.files.write().insert(Arc::new(file))
    }

    /// Remove the file with the specified node ID.
    fn remove_file(&self, fh: u64) {
        self.files.write().remove(fh);
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
    inodes_by_inode_nr: Option<HashMap<lx::ino_t, (Arc<VirtioFsInode>, u64)>>,
}

impl InodeMap {
    /// Create a new `InodeMap`.
    pub fn new(supports_stable_file_id: bool) -> Self {
        // TODO: Once multiple volumes are supported, the inodes_by_inode_nr map should be per
        // volume.
        Self {
            inodes_by_node_id: HandleMap::new(),
            inodes_by_inode_nr: if supports_stable_file_id {
                Some(HashMap::new())
            } else {
                None
            },
        }
    }

    /// Get an inode with the specified FUSE node ID.
    pub fn get(&self, node_id: u64) -> Option<Arc<VirtioFsInode>> {
        let inode = self.inodes_by_node_id.get(node_id)?;
        Some(Arc::clone(inode))
    }

    /// Insert an inode into the map, returning its node ID.
    pub fn insert(&mut self, inode: VirtioFsInode) -> (Arc<VirtioFsInode>, u64) {
        // If stable inode numbers are supported, look for the inode by its number.
        if let Some(inodes_by_inode_nr) = self.inodes_by_inode_nr.as_mut() {
            match inodes_by_inode_nr.entry(inode.inode_nr()) {
                Entry::Occupied(entry) => {
                    // Inode found; increment its count and return the existing FUSE node ID.
                    let new_path = inode.clone_path();
                    let (inode, node_id) = entry.get();
                    inode.lookup(new_path);
                    return (Arc::clone(inode), *node_id);
                }
                Entry::Vacant(entry) => {
                    // Inode not found, so insert it into both maps.
                    let inode = Arc::new(inode);
                    let node_id = self.inodes_by_node_id.insert(Arc::clone(&inode));
                    entry.insert((Arc::clone(&inode), node_id));
                    return (inode, node_id);
                }
            }
        }

        // No support for stable inode numbers, so just use node ID.
        let inode = Arc::new(inode);
        let node_id = self.inodes_by_node_id.insert(Arc::clone(&inode));
        (inode, node_id)
    }

    /// Remove an inode with the specified FUSE node ID from the map.
    pub fn remove(&mut self, node_id: u64) {
        let inode = self.inodes_by_node_id.remove(node_id).unwrap();
        if let Some(inodes_by_inode_nr) = self.inodes_by_inode_nr.as_mut() {
            inodes_by_inode_nr.remove(&inode.inode_nr());
        }
    }

    /// Clears the map, preserving the root inode.
    pub fn clear(&mut self) {
        let root_inode = Arc::clone(self.inodes_by_node_id.get(FUSE_ROOT_ID).unwrap());
        self.inodes_by_node_id.clear();

        // Re-insert the root inode.
        assert!(self.inodes_by_node_id.insert(Arc::clone(&root_inode)) == FUSE_ROOT_ID);

        // Clear the inode number map if it's supported.
        if let Some(inodes_by_inode_nr) = self.inodes_by_inode_nr.as_mut() {
            inodes_by_inode_nr.clear();
            inodes_by_inode_nr.insert(root_inode.inode_nr(), (root_inode, FUSE_ROOT_ID));
        }
    }
}
