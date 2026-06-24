// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![expect(missing_docs)]
#![cfg(any(windows, target_os = "linux"))]

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
use zerocopy::FromZeros;

// TODO: Make these configurable.
// FUSE likes to spam getattr a lot, so having a small timeout on the attributes avoids excessive
// calls. It also means that a lookup/stat sequence can use the attributes returned by lookup
// rather than having to call getattr.
const ATTRIBUTE_TIMEOUT: Duration = Duration::from_millis(1);

// Entry timeout must be zero, because on rename existing entries for the child being renamed do
// not get updated and would stop working. Having a zero timeout forces a new lookup which will
// update the path.
const ENTRY_TIMEOUT: Duration = Duration::from_secs(0);

/// Reserved file handle returned by `open_dir` on the synthetic aggregate root.
///
/// `read_dir`/`read_dir_plus`/`release_dir` recognize this sentinel and service
/// it from the root registry rather than the (real-file) handle map. `u64::MAX`
/// can never collide with a real handle because `HandleMap` allocates starting
/// at 1 and only increments.
const SYNTHETIC_ROOT_FH: u64 = u64::MAX;

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

/// Shared mutable state behind a [`VirtioFs`] handle.
struct VirtioFsInner {
    inodes: RwLock<InodeMap>,
    files: RwLock<HandleMap<Arc<VirtioFsFile>>>,
    /// Aggregated roots; empty and unused in direct (single-root) mode.
    roots: RwLock<RootRegistry>,
    readonly: bool,
    /// When true, node 1 is a synthetic directory whose children are the entries
    /// in `roots`. When false, node 1 is a real inode at a single volume root
    /// (legacy single-share behavior).
    aggregate: bool,
    /// When true, child roots are advertised with `FUSE_ATTR_SUBMOUNT` so the
    /// guest kernel gives each share its own `st_dev`. Only meaningful in
    /// aggregate mode and only honored once `FUSE_SUBMOUNTS` is negotiated.
    submounts: bool,
}

/// Implementation of the virtio-fs file system.
///
/// Cheaply clonable: all state lives behind an `Arc`, so a device host can keep
/// a handle to call [`VirtioFs::add_root`]/[`VirtioFs::remove_root`] after the
/// device has been constructed and handed to a [`fuse::Session`].
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

        // In aggregate mode, advertise submounts so the guest gives each child
        // root its own st_dev when crossing into it.
        if self.inner.submounts && info.capable() & FUSE_SUBMOUNTS != 0 {
            info.want |= FUSE_SUBMOUNTS;
        }
    }

    fn get_attr(&self, request: &Request, flags: u32, fh: u64) -> lx::Result<fuse_attr_out> {
        let node_id = request.node_id();
        // If a file handle is specified, get the attributes from the open file. This is faster on
        // Windows and works if the file was deleted.
        let attr = if flags & FUSE_GETATTR_FH != 0 {
            let file = self.get_file(fh)?;
            file.get_attr()?
        } else if self.is_synthetic_root(node_id) {
            Self::synthetic_root_attr()
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
        // Windows and works if the file was deleted.
        let statx = if getattr_flags & FUSE_GETATTR_FH != 0 {
            let file = self.get_file(fh)?;
            file.get_statx()?
        } else if self.is_synthetic_root(node_id) {
            Self::synthetic_root_statx(mask)
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
            // Block truncation and other modifications on readonly filesystems
            if arg.valid & !(FATTR_FH | FATTR_LOCKOWNER) != 0 {
                self.check_writable()?;
            }
            file.set_attr(arg, request.uid())?;
            file.get_attr()?
        } else {
            let inode = self.get_inode(node_id)?;
            // Block truncation and other modifications on readonly filesystems
            if arg.valid & !(FATTR_FH | FATTR_LOCKOWNER) != 0 {
                self.check_writable()?;
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
        let inode = self.get_inode(request.node_id())?;
        self.check_writable()?;
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
        self.check_writable()?;
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
        self.check_writable()?;
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
        self.check_writable()?;
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
        self.check_writable()?;
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
        self.check_writable()?;
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

    fn read_dir(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        if arg.fh == SYNTHETIC_ROOT_FH {
            return self.read_synthetic_root_dir(arg.offset, arg.size, false);
        }
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, false)
    }

    fn read_dir_plus(&self, _request: &Request, arg: &fuse_read_in) -> lx::Result<Vec<u8>> {
        if arg.fh == SYNTHETIC_ROOT_FH {
            return self.read_synthetic_root_dir(arg.offset, arg.size, true);
        }
        let file = self.get_file(arg.fh)?;
        file.read_dir(self, arg.offset, arg.size, true)
    }

    fn release_dir(&self, request: &Request, arg: &fuse_release_in) -> lx::Result<()> {
        if arg.fh == SYNTHETIC_ROOT_FH {
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
        let inode = self.get_inode(request.node_id())?;
        let new_inode = self.get_inode(new_dir)?;
        // A rename cannot cross aggregated volume boundaries.
        if inode.volume_id() != new_inode.volume_id() {
            return Err(lx::Error::EXDEV);
        }
        self.check_writable()?;
        inode.rename(name, &new_inode, new_name, flags)
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
        self.check_writable()?;
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
        self.check_writable()?;
        inode.remove_xattr(name)
    }

    fn destroy(&self) {
        // To get the file system ready for re-mount, clean out any open files and leaked inodes.
        self.inner.files.write().clear();
        self.inner.inodes.write().clear();
    }
}

impl VirtioFs {
    /// Check if the filesystem is readonly and return EROFS if so.
    fn check_writable(&self) -> lx::Result<()> {
        if self.inner.readonly {
            Err(lx::Error::EROFS)
        } else {
            Ok(())
        }
    }

    /// Check whether the open flags are permitted on a read-only filesystem.
    fn check_open_readonly(&self, inode: &VirtioFsInode, flags: u32) -> lx::Result<()> {
        if !self.inner.readonly {
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
        let readonly = mount_options.is_some_and(|o| o.is_readonly());
        let volume = if let Some(mount_options) = mount_options {
            mount_options.new_volume(root_path)
        } else {
            lxutil::LxVolume::new(root_path)
        }?;
        let mut inodes = InodeMap::new(volume.supports_stable_file_id(), false);
        let (root_inode, _) = VirtioFsInode::new(Arc::new(volume), 0, PathBuf::new())?;
        assert!(inodes.insert(root_inode).1 == FUSE_ROOT_ID);
        Ok(Self {
            inner: Arc::new(VirtioFsInner {
                inodes: RwLock::new(inodes),
                files: RwLock::new(HandleMap::new()),
                roots: RwLock::new(RootRegistry::new()),
                readonly,
                aggregate: false,
                submounts: false,
            }),
        })
    }

    /// Create a new, empty aggregate virtio-fs.
    ///
    /// Node 1 is a synthetic, read-only directory; use [`Self::add_root`] to
    /// expose host folders as named children. When `submounts` is set, each
    /// child is advertised with `FUSE_ATTR_SUBMOUNT` so the guest kernel gives
    /// it a distinct `st_dev` (only honored once `FUSE_SUBMOUNTS` is negotiated).
    pub fn new_aggregate(readonly: bool, submounts: bool) -> Self {
        Self {
            inner: Arc::new(VirtioFsInner {
                // Inode numbers are deduplicated per volume (see `InodeMap`), so
                // enable the stable-id map and key it by `(volume_id, ino)`.
                inodes: RwLock::new(InodeMap::new(true, true)),
                files: RwLock::new(HandleMap::new()),
                roots: RwLock::new(RootRegistry::new()),
                readonly,
                aggregate: true,
                submounts,
            }),
        }
    }

    /// Expose a host folder as a named child of the synthetic root.
    ///
    /// Only valid in aggregate mode. Returns `EEXIST` if a root with the same
    /// name already exists, or `EINVAL` on a direct-mode file system.
    pub fn add_root(
        &self,
        name: &str,
        root_path: impl AsRef<Path>,
        mount_options: Option<&LxVolumeOptions>,
    ) -> lx::Result<()> {
        if !self.inner.aggregate {
            return Err(lx::Error::EINVAL);
        }

        let mut roots = self.inner.roots.write();
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

    /// Remove a previously added root by name.
    ///
    /// In-flight inodes beneath the root remain valid until the guest forgets
    /// them (each holds its own volume reference); the name simply stops
    /// appearing in the synthetic root. Returns `ENOENT` if no such root exists.
    pub fn remove_root(&self, name: &str) -> lx::Result<()> {
        if !self.inner.aggregate {
            return Err(lx::Error::EINVAL);
        }

        let mut roots = self.inner.roots.write();
        let before = roots.entries.len();
        roots.entries.retain(|e| e.name != name);
        if roots.entries.len() == before {
            Err(lx::Error::ENOENT)
        } else {
            Ok(())
        }
    }

    /// Returns true if `node_id` refers to the synthetic aggregate root.
    fn is_synthetic_root(&self, node_id: u64) -> bool {
        self.inner.aggregate && node_id == FUSE_ROOT_ID
    }

    /// Attributes of the synthetic aggregate root directory.
    fn synthetic_root_attr() -> fuse_attr {
        let mut attr = fuse_attr::new_zeroed();
        attr.ino = FUSE_ROOT_ID;
        attr.mode = lx::S_IFDIR | 0o555;
        attr.nlink = 2;
        attr.blksize = 512;
        attr
    }

    /// Extended attributes of the synthetic aggregate root directory.
    fn synthetic_root_statx(mask: lx::StatExMask) -> fuse_statx {
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
    fn lookup_synthetic_root(&self, name: &lx::LxStr) -> lx::Result<fuse_entry_out> {
        let name_bytes = name.as_bytes();
        let (volume, volume_id) = {
            let roots = self.inner.roots.read();
            let entry = roots
                .entries
                .iter()
                .find(|e| e.name.as_bytes() == name_bytes)
                .ok_or(lx::Error::ENOENT)?;
            (Arc::clone(&entry.volume), entry.volume_id)
        };

        let (inode, stat) = VirtioFsInode::new(volume, volume_id, PathBuf::new())?;
        let (_, node_id) = self.insert_inode(inode);
        let mut attr = util::stat_to_fuse_attr(&stat);
        if self.inner.submounts {
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
    fn read_synthetic_root_dir(&self, offset: u64, size: u32, plus: bool) -> lx::Result<Vec<u8>> {
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
                        let roots = self.inner.roots.read();
                        roots.entries.get((n - 2) as usize).map(|e| {
                            (e.name.clone(), Arc::clone(&e.volume), e.volume_id)
                        })
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
            let (_, node_id) = self.insert_inode(inode);
            let mut attr = util::stat_to_fuse_attr(&stat);
            if self.inner.submounts {
                attr.flags |= FUSE_ATTR_SUBMOUNT;
            }
            let entry = fuse_entry_out::new(node_id, ENTRY_TIMEOUT, ATTRIBUTE_TIMEOUT, attr);
            Ok(buffer.dir_entry_plus(name, next_off, entry))
        } else {
            // Plain readdir: report the directory using the volume root's real
            // inode number, falling back to the volume id if it is inaccessible.
            let ino = volume
                .lstat(&PathBuf::new())
                .map(|s| s.inode_nr)
                .unwrap_or(volume_id as lx::ino_t);
            Ok(buffer.dir_entry(name, ino, next_off, DT_DIR))
        }
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
        self.check_writable()?;
        inode.unlink(name, flags)
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
    inodes_by_inode_nr: Option<HashMap<(u32, lx::ino_t), (Arc<VirtioFsInode>, u64)>>,
    /// When true, node 1 is synthetic and not stored in this map, so node IDs
    /// are allocated starting at 2 and `clear` does not preserve a real root.
    aggregate: bool,
}

impl InodeMap {
    /// Create a new `InodeMap`.
    pub fn new(supports_stable_file_id: bool, aggregate: bool) -> Self {
        Self {
            inodes_by_node_id: if aggregate {
                HandleMap::starting_at(FUSE_ROOT_ID + 1)
            } else {
                HandleMap::new()
            },
            inodes_by_inode_nr: if supports_stable_file_id {
                Some(HashMap::new())
            } else {
                None
            },
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
        // If stable inode numbers are supported, look for the inode by its number.
        if let Some(inodes_by_inode_nr) = self.inodes_by_inode_nr.as_mut() {
            match inodes_by_inode_nr.entry((inode.volume_id(), inode.inode_nr())) {
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
            inodes_by_inode_nr.remove(&(inode.volume_id(), inode.inode_nr()));
        }
    }

    /// Clears the map, preserving the root inode.
    pub fn clear(&mut self) {
        if self.aggregate {
            // Node 1 is synthetic and not stored here; drop everything and resume
            // allocating node IDs after the reserved root id.
            self.inodes_by_node_id.clear();
            self.inodes_by_node_id.next_handle = FUSE_ROOT_ID + 1;
            if let Some(inodes_by_inode_nr) = self.inodes_by_inode_nr.as_mut() {
                inodes_by_inode_nr.clear();
            }
            return;
        }

        let root_inode = Arc::clone(self.inodes_by_node_id.get(FUSE_ROOT_ID).unwrap());
        self.inodes_by_node_id.clear();

        // Re-insert the root inode.
        assert!(self.inodes_by_node_id.insert(Arc::clone(&root_inode)) == FUSE_ROOT_ID);

        // Clear the inode number map if it's supported.
        if let Some(inodes_by_inode_nr) = self.inodes_by_inode_nr.as_mut() {
            inodes_by_inode_nr.clear();
            inodes_by_inode_nr.insert(
                (root_inode.volume_id(), root_inode.inode_nr()),
                (root_inode, FUSE_ROOT_ID),
            );
        }
    }
}

#[cfg(test)]
mod aggregate_tests {
    use super::*;

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
            let roots = fs.inner.roots.read();
            assert_eq!(roots.entries.len(), 2);
            assert_ne!(roots.entries[0].volume_id, 0);
            assert_ne!(roots.entries[0].volume_id, roots.entries[1].volume_id);
        }

        // Removal drops only the named root.
        fs.remove_root("share_a").unwrap();
        assert_eq!(
            fs.remove_root("share_a").unwrap_err(),
            lx::Error::ENOENT
        );
        assert_eq!(fs.inner.roots.read().entries.len(), 1);
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
        assert!(entry.nodeid > FUSE_ROOT_ID);
    }
}

