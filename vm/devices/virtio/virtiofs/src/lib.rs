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
use inode::AggregateState;
use inode::DedupKey;
use inode::SyntheticChild;
use inode::VirtioFsInode;
use inode::Volume;
pub use lxutil::LxVolumeOptions;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::collections::HashSet;
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
    readonly: bool,
    /// `Some` only for aggregate devices. Held so that `Drop` can mark the
    /// state as `TearingDown`, causing any externally-held
    /// [`VirtiofsAggregateHandle`] to start rejecting `add_child`.
    aggregate_state: Option<Arc<AggregateState>>,
}

impl Drop for VirtioFs {
    fn drop(&mut self) {
        if let Some(state) = &self.aggregate_state {
            state.mark_tearing_down();
        }
    }
}

/// Handle to a live aggregate virtio-fs device that can append new
/// children after construction. Obtained from
/// [`VirtioFs::aggregate_handle`]; `add_child` is visible to the guest on
/// the next LOOKUP/READDIR. Cloning shares the underlying state.
#[derive(Clone)]
pub struct VirtiofsAggregateHandle {
    state: Arc<AggregateState>,
}

impl VirtiofsAggregateHandle {
    /// Append a new child to the live aggregate.
    ///
    /// Errors:
    /// - `EAGAIN` — the owning device has begun tearing down.
    /// - `EINVAL` — `child.name` failed validation, or its readonly setting
    ///   does not match the aggregate's.
    /// - `EEXIST` — a child with this name is already present.
    /// - any `lx::Error` propagated from `LxVolume` construction.
    ///
    /// Slow operations (volume creation, root stat) run outside the
    /// children write lock so they do not block concurrent LOOKUP/READDIR.
    pub fn add_child(&self, child: VirtioFsChild) -> lx::Result<()> {
        inode::validate_child_name_bytes(child.name.as_bytes())?;

        // Fast-fail without paying for volume construction if the device is
        // already tearing down. The duplicate-name check is enforced
        // authoritatively (under the lock) by `AggregateState::add_child`.
        if !self.state.is_active() {
            return Err(lx::Error::EAGAIN);
        }

        // Reject a readonly mismatch before building the (expensive) volume.
        // The aggregate's readonly value is fixed at construction. The check
        // is repeated authoritatively under the lock in
        // `AggregateState::add_child`.
        let child_readonly = child.readonly();
        if child_readonly != self.state.readonly() {
            return Err(lx::Error::EINVAL);
        }

        let volume = child.build_volume()?;
        let name = lx::LxString::from_vec(child.name.into_bytes());

        self.state.add_child(name, volume, child_readonly)
    }
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

        // Opt in to FUSE_SUBMOUNTS so the kernel honors the
        // `FUSE_ATTR_SUBMOUNT` flag returned on the root inode of each
        // child of an aggregate (multi-path) virtio-fs root. The flag is
        // harmless when single-root mounts never set the attr bit.
        //
        // N.B. FUSE_SUBMOUNTS is best-effort isolation: when honored it
        // gives each child its own superblock (distinct `st_dev`). But
        // whether the kernel actually instantiates the submount depends on
        // guest-side mount mechanics (e.g. WSL bind-mounts each child from a
        // subpath of the synthetic root, which can pin the child dentry as a
        // plain, non-automount dentry so the submount never fires). The
        // correctness of the aggregate therefore does *not* rely on
        // submounts: child inode numbers are namespaced per share (see
        // `Volume::namespaced_ino`) so `(st_dev, st_ino)` collisions across
        // children are avoided even under a single shared superblock.
        if info.capable() & FUSE_SUBMOUNTS != 0 {
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
        let inode = self.get_inode(request.node_id())?;
        self.lookup_helper(&inode, name)
    }

    fn forget(&self, node_id: u64, lookup_count: u64) {
        // The FUSE protocol guarantees that the kernel never forgets the
        // root inode. Defend against malformed/forged guest requests
        // anyway: dropping the root would corrupt all subsequent lookups
        // (every path resolves through FUSE_ROOT_ID).
        if node_id == FUSE_ROOT_ID {
            // A buggy or malicious guest can send this repeatedly, so rate-limit
            // the warning to avoid unbounded log spam.
            tracelimit::warn_ratelimited!(lookup_count, "ignoring forget on root inode");
            return;
        }
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
        let file = VirtioFsFile::new_real(file, new_inode);
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
        self.check_writable()?;
        inode.rename(name, &new_inode, new_name, flags)?;
        // On path-keyed (non-stable-id) volumes a rename moves data between
        // paths without preserving inode identity, so detach both the source
        // path (now vacated) and the destination path (its prior occupant, if
        // any, was replaced) from any node ids they referenced. This avoids a
        // later lookup aliasing a stale node id. (Descendants of a renamed
        // directory keep stale path keys; this is a known, narrow limitation
        // — FAT cannot preserve inode identity across rename regardless.)
        let mut inodes = self.inodes.write();
        if let Some(key) = inode.child_path_dedup_key(name) {
            inodes.evict_dedup_key(&key);
        }
        if let Some(key) = new_inode.child_path_dedup_key(new_name) {
            inodes.evict_dedup_key(&key);
        }
        Ok(())
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
        self.files.write().clear();
        self.inodes.write().clear();
    }
}

impl VirtioFs {
    /// Check if the filesystem is readonly and return EROFS if so.
    fn check_writable(&self) -> lx::Result<()> {
        if self.readonly {
            Err(lx::Error::EROFS)
        } else {
            Ok(())
        }
    }

    /// Check whether the open flags are permitted on a read-only filesystem.
    fn check_open_readonly(&self, inode: &VirtioFsInode, flags: u32) -> lx::Result<()> {
        if !self.readonly {
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
        let volume = Volume::open(root_path, mount_options)?;
        let (root_inode, _) = VirtioFsInode::new(volume, PathBuf::new())?;
        Ok(Self::from_root_inode(root_inode, readonly, None))
    }

    /// Assemble a `VirtioFs` from an already-built root inode. Shared tail
    /// of `new` and `new_aggregate`: inserts `root_inode` at
    /// [`FUSE_ROOT_ID`] and initializes the remaining fields.
    fn from_root_inode(
        root_inode: VirtioFsInode,
        readonly: bool,
        aggregate_state: Option<Arc<AggregateState>>,
    ) -> Self {
        let mut inodes = InodeMap::new();
        assert!(inodes.insert(root_inode).1 == FUSE_ROOT_ID);
        Self {
            inodes: RwLock::new(inodes),
            files: RwLock::new(HandleMap::new()),
            readonly,
            aggregate_state,
        }
    }

    /// Create a new virtio-fs that aggregates multiple host paths under a
    /// synthetic read-only root. Each child appears at `/<name>` and is
    /// auto-mounted as a separate Linux superblock via `FUSE_ATTR_SUBMOUNT`
    /// (requires guest kernel negotiating `FUSE_SUBMOUNTS`, Linux >= 5.10).
    /// All children must share the same readonly setting.
    ///
    /// Use [`VirtioFs::aggregate_handle`] to obtain a
    /// [`VirtiofsAggregateHandle`] for appending further children to the
    /// live device.
    pub fn new_aggregate(children: Vec<VirtioFsChild>) -> lx::Result<Self> {
        if children.is_empty() {
            return Err(lx::Error::EINVAL);
        }

        // Validate child names + uniqueness, and determine the shared
        // readonly setting, before opening any host volumes.
        let mut seen: HashSet<&[u8]> = HashSet::with_capacity(children.len());
        let mut readonly: Option<bool> = None;
        for child in &children {
            inode::validate_child_name_bytes(child.name.as_bytes())?;
            if !seen.insert(child.name.as_bytes()) {
                tracing::warn!(
                    name = ?child.name,
                    "duplicate child name in aggregate virtio-fs"
                );
                return Err(lx::Error::EEXIST);
            }
            let child_readonly = child.readonly();
            match readonly {
                None => readonly = Some(child_readonly),
                Some(r) if r != child_readonly => {
                    tracing::warn!(
                        "aggregate virtio-fs children must all share the same readonly setting"
                    );
                    return Err(lx::Error::EINVAL);
                }
                _ => (),
            }
        }
        let readonly = readonly.unwrap_or(false);

        // Build a `Volume` per child.
        let mut synthetic_children: Vec<SyntheticChild> = Vec::with_capacity(children.len());
        for child in children {
            let volume = child.build_volume()?;
            synthetic_children.push(SyntheticChild {
                name: lx::LxString::from_vec(child.name.into_bytes()),
                volume,
            });
        }

        let state = AggregateState::new(synthetic_children, readonly);
        let root_inode = VirtioFsInode::new_synthetic_root(Arc::clone(&state));
        Ok(Self::from_root_inode(root_inode, readonly, Some(state)))
    }

    /// Obtain a [`VirtiofsAggregateHandle`] for appending children to a
    /// live aggregate device. Returns `None` for non-aggregate devices
    /// (those created via [`VirtioFs::new`]). Each call returns a fresh
    /// clone that shares the underlying aggregate state.
    pub fn aggregate_handle(&self) -> Option<VirtiofsAggregateHandle> {
        self.aggregate_state
            .as_ref()
            .map(|state| VirtiofsAggregateHandle {
                state: Arc::clone(state),
            })
    }

    /// Perform lookup on a specified directory inode.
    pub(crate) fn lookup_helper(
        &self,
        inode: &VirtioFsInode,
        name: &lx::LxStr,
    ) -> lx::Result<fuse_entry_out> {
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
        inode.unlink(name, flags)?;
        // On path-keyed (non-stable-id) volumes the path is the inode's
        // identity, so detach it from any node id now that it is gone; a
        // later create at the same path must not alias the removed inode.
        if let Some(key) = inode.child_path_dedup_key(name) {
            self.inodes.write().evict_dedup_key(&key);
        }
        Ok(())
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

/// Assigns node IDs to inodes, and keeps track of in-use inodes by their actual
/// (volume_id, inode_nr) pair.
///
/// We cannot use the real inode number as the FUSE node ID:
/// - FUSE node ID 1 is reserved for the root, so this would break if a file system used that inode
///   number.
/// - Multiple volumes can share the same inode number value, so the key must include the
///   volume id.
struct InodeMap {
    inodes_by_node_id: HandleMap<Arc<VirtioFsInode>>,
    /// Maps a [`DedupKey`] to the registered inode and its FUSE node id, for
    /// inodes eligible for deduplication. Stable-id volumes key by inode
    /// number ([`DedupKey::Ino`]); volumes that recycle inode numbers
    /// (FAT/exFAT) key by path ([`DedupKey::Path`]). The synthetic root and
    /// empty-path submount/volume roots are not entered here.
    inodes_by_key: HashMap<DedupKey, (Arc<VirtioFsInode>, u64)>,
}

impl InodeMap {
    pub fn new() -> Self {
        Self {
            inodes_by_node_id: HandleMap::new(),
            inodes_by_key: HashMap::new(),
        }
    }

    /// Get an inode with the specified FUSE node ID.
    pub fn get(&self, node_id: u64) -> Option<Arc<VirtioFsInode>> {
        let inode = self.inodes_by_node_id.get(node_id)?;
        Some(Arc::clone(inode))
    }

    /// Insert an inode into the map, returning its node ID.
    pub fn insert(&mut self, inode: VirtioFsInode) -> (Arc<VirtioFsInode>, u64) {
        // If this inode has a valid dedup key, look for it in the map.
        if let Some(key) = inode.dedup_key() {
            match self.inodes_by_key.entry(key) {
                Entry::Occupied(entry) => {
                    // Inode found; increment its count and return the existing FUSE node ID.
                    let new_path = inode.clone_path();
                    let (existing, node_id) = entry.get();
                    existing.lookup(new_path);
                    return (Arc::clone(existing), *node_id);
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

        // Inode is not eligible for dedup (synthetic root or
        // non-stable-inode volume); just allocate a fresh node id.
        let inode = Arc::new(inode);
        let node_id = self.inodes_by_node_id.insert(Arc::clone(&inode));
        (inode, node_id)
    }

    /// Remove an inode with the specified FUSE node ID from the map.
    pub fn remove(&mut self, node_id: u64) {
        let inode = self.inodes_by_node_id.remove(node_id).unwrap();
        if let Some(key) = inode.dedup_key() {
            // Only drop the by-key entry if it still points at THIS node.
            // For path-keyed volumes a delete+recreate (or an explicit
            // `evict_dedup_key`) can repoint the path to a newer inode while
            // this (older) one lingers behind a live fd or inotify watch;
            // removing it unconditionally would orphan that newer inode.
            if let Entry::Occupied(entry) = self.inodes_by_key.entry(key) {
                if entry.get().1 == node_id {
                    entry.remove();
                }
            }
        }
    }

    /// Detach a [`DedupKey::Path`] entry from whatever inode it currently
    /// maps to, leaving that inode in `inodes_by_node_id` (a live fd or
    /// inotify watch may still reference it) but no longer reachable for
    /// dedup. A subsequent create at the same path therefore gets a fresh
    /// node id rather than aliasing the removed/renamed file.
    ///
    /// No-op for [`DedupKey::Ino`] keys: stable-id volumes get a different
    /// inode number on recreate, so the stale key cannot alias.
    pub fn evict_dedup_key(&mut self, key: &DedupKey) {
        if matches!(key, DedupKey::Path(..)) {
            self.inodes_by_key.remove(key);
        }
    }

    /// Clears the map, preserving the root inode.
    pub fn clear(&mut self) {
        let root_inode = Arc::clone(self.inodes_by_node_id.get(FUSE_ROOT_ID).unwrap());
        self.inodes_by_node_id.clear();

        // Re-insert the root inode.
        assert!(self.inodes_by_node_id.insert(Arc::clone(&root_inode)) == FUSE_ROOT_ID);

        // Rebuild the dedup map containing only the root, if eligible.
        self.inodes_by_key.clear();
        if let Some(key) = root_inode.dedup_key() {
            self.inodes_by_key.insert(key, (root_inode, FUSE_ROOT_ID));
        }
    }
}

/// Description of one child path to expose at a named subdirectory of an
/// aggregate (multi-path) virtio-fs root. See [`VirtioFs::new_aggregate`].
pub struct VirtioFsChild {
    /// Name of the child as it appears in the synthetic root directory.
    /// Must be non-empty, not contain `/` or NUL, and not be `.` or `..`.
    pub name: String,
    /// Host path to use as the root of this child volume.
    pub root_path: PathBuf,
    /// Optional mount options. If supplied, all children passed in the
    /// same call must agree on the `readonly` setting.
    pub options: Option<LxVolumeOptions>,
}

impl VirtioFsChild {
    /// The readonly setting for this child (defaults to read-write when no
    /// options are supplied).
    fn readonly(&self) -> bool {
        self.options.as_ref().is_some_and(|o| o.is_readonly())
    }

    /// Open the backing host volume for this child. Borrows `self` so the
    /// caller can subsequently move `self.name` after the (slow) volume
    /// construction succeeds.
    fn build_volume(&self) -> lx::Result<Arc<Volume>> {
        Volume::open_aggregate_child(&self.root_path, self.options.as_ref())
    }
}
