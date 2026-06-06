// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::inode::VirtioFsInode;
use crate::util;
use fuse::DirEntryWriter;
use fuse::protocol::fuse_attr;
use fuse::protocol::fuse_entry_out;
use fuse::protocol::fuse_setattr_in;
use fuse::protocol::fuse_statx;
use fuse::protocol::*;
use lxutil::LxFile;
use parking_lot::RwLock;
use std::sync::Arc;
use zerocopy::FromZeros;

/// An open file handle backed by a real `LxFile`.
struct RealFile {
    file: RwLock<LxFile>,
    inode: Arc<VirtioFsInode>,
}

/// An open handle on the synthetic root of an aggregate virtio-fs.
///
/// Holds only the inode; reads stream the in-memory child list.
struct SyntheticRootDirFile {
    inode: Arc<VirtioFsInode>,
}

/// The kind of open file backing a virtio-fs file handle.
enum FileKind {
    /// A file or directory handle backed by a real `LxFile`.
    ///
    /// Boxed to keep the enum small: `LxFile` is large and the
    /// `SyntheticRootDir` variant only holds an `Arc`.
    Real(Box<RealFile>),
    /// An open handle on the synthetic root of an aggregate virtio-fs.
    SyntheticRootDir(SyntheticRootDirFile),
}

/// Implements file callbacks for virtio-fs.
pub struct VirtioFsFile {
    kind: FileKind,
}

impl VirtioFsFile {
    /// Create a new file handle backed by a real `LxFile`.
    pub fn new_real(file: LxFile, inode: Arc<VirtioFsInode>) -> Self {
        Self {
            kind: FileKind::Real(Box::new(RealFile {
                file: RwLock::new(file),
                inode,
            })),
        }
    }

    /// Create a new file handle for the synthetic root directory of an
    /// aggregate virtio-fs.
    pub fn new_synthetic_root_dir(inode: Arc<VirtioFsInode>) -> Self {
        debug_assert!(inode.is_synthetic_root());
        Self {
            kind: FileKind::SyntheticRootDir(SyntheticRootDirFile { inode }),
        }
    }

    /// Gets the attributes of the open file.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        match &self.kind {
            FileKind::Real(f) => f.get_attr(),
            FileKind::SyntheticRootDir(f) => f.inode.get_attr(),
        }
    }

    /// Gets the statx details for the open file.
    pub fn get_statx(&self) -> lx::Result<fuse_statx> {
        match &self.kind {
            FileKind::Real(f) => f.get_statx(),
            FileKind::SyntheticRootDir(f) => f.inode.get_statx(),
        }
    }

    /// Sets the attributes of the open file.
    pub fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<()> {
        match &self.kind {
            FileKind::Real(f) => f.set_attr(arg, request_uid),
            FileKind::SyntheticRootDir(_) => Err(lx::Error::EROFS),
        }
    }

    /// Read data from the file.
    pub fn read(&self, buffer: &mut [u8], offset: u64) -> lx::Result<usize> {
        match &self.kind {
            FileKind::Real(f) => f.read(buffer, offset),
            // The synthetic root is a directory.
            FileKind::SyntheticRootDir(_) => Err(lx::Error::EISDIR),
        }
    }

    /// Write data to the file.
    pub fn write(&self, buffer: &[u8], offset: u64, thread_uid: lx::uid_t) -> lx::Result<usize> {
        match &self.kind {
            FileKind::Real(f) => f.write(buffer, offset, thread_uid),
            FileKind::SyntheticRootDir(_) => Err(lx::Error::EISDIR),
        }
    }

    /// Read directory contents.
    pub fn read_dir(
        &self,
        fs: &super::VirtioFs,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        match &self.kind {
            FileKind::Real(f) => f.read_dir(fs, offset, size, plus),
            FileKind::SyntheticRootDir(f) => f.read_dir(fs, offset, size, plus),
        }
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        match &self.kind {
            FileKind::Real(f) => f.file.read().fsync(data_only),
            // Nothing to flush for a synthetic directory.
            FileKind::SyntheticRootDir(_) => Ok(()),
        }
    }
}

impl RealFile {
    fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.file.read().fstat()?.into();
        let mut attr = util::stat_to_fuse_attr(&stat);
        attr.ino = self.inode.namespaced_ino(attr.ino);
        Ok(attr)
    }

    fn get_statx(&self) -> lx::Result<fuse_statx> {
        let statx = self.file.read().fstat()?;
        let mut sx = util::statx_to_fuse_statx(&statx);
        sx.ino = self.inode.namespaced_ino(sx.ino);
        Ok(sx)
    }

    fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<()> {
        let attr = util::fuse_set_attr_to_lxutil(arg, request_uid);

        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared
        // depending on the attributes being set. Lxutil takes care of that on Windows (and Linux
        // does it naturally).
        self.file.read().set_attr(attr)
    }

    fn read(&self, buffer: &mut [u8], offset: u64) -> lx::Result<usize> {
        self.file.read().pread(buffer, offset as lx::off_t)
    }

    fn write(&self, buffer: &[u8], offset: u64, thread_uid: lx::uid_t) -> lx::Result<usize> {
        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared on
        // write. Lxutil takes care of that on Windows (and Linux does it naturally).
        self.file
            .read()
            .pwrite(buffer, offset as lx::off_t, thread_uid)
    }

    fn read_dir(
        &self,
        fs: &super::VirtioFs,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        let mut buffer = Vec::with_capacity(size as usize);
        let mut entry_count: u32 = 0;
        // Namespace `.`/`..` (and child entries below) so plain READDIR `d_ino`
        // values match the inode numbers reported by LOOKUP/GETATTR/READDIRPLUS
        // for the same inodes.
        let self_inode_nr = self.inode.namespaced_ino(self.inode.inode_nr());
        let mut file = self.file.write();
        file.read_dir(offset as lx::off_t, |entry| {
            entry_count += 1;
            let get_child_fuse_entry = || -> lx::Result<Option<fuse_entry_out>> {
                match fs.lookup_helper(&self.inode, &entry.name) {
                    Ok(e) => Ok(Some(e)),
                    Err(err) => {
                        // Ignore entries that are inaccessible to the user or deleted.
                        // ENOENT can occur if a file was deleted between enumeration
                        // and lookup (e.g., when deleting files in a loop while
                        // enumerating the directory).
                        if err.value() == lx::EACCES || err.value() == lx::ENOENT {
                            Ok(None)
                        } else {
                            Err(err)
                        }
                    }
                }
            };
            // If readdirplus is being used, do a lookup on all items except the . and .. entries.
            if plus {
                let fuse_entry = if entry.name == "." || entry.name == ".." {
                    let mut e = fuse_entry_out::new_zeroed();
                    e.attr.ino = self_inode_nr;
                    e.attr.mode = (entry.file_type as u32) << 12;
                    e
                } else {
                    if !buffer.check_dir_entry_plus(&entry.name) {
                        return Ok(false);
                    }

                    match get_child_fuse_entry()? {
                        Some(e) => e,
                        None => {
                            // Ignore entries that are inaccessible to the user.
                            entry_count -= 1;
                            return Ok(true);
                        }
                    }
                };

                Ok(buffer.dir_entry_plus(&entry.name, entry.offset as u64, fuse_entry))
            } else {
                // Use the current file's inode number for . and .. entries.
                // On Windows inode_nr is 0 for these; on Linux it may be
                // non-zero, so check by name rather than relying on the
                // inode number to identify them.
                let inode_nr = if entry.name == "." || entry.name == ".." {
                    self_inode_nr
                } else {
                    if get_child_fuse_entry()?.is_none() {
                        // Ignore entries that are inaccessible to the user.
                        entry_count -= 1;
                        return Ok(true);
                    }
                    self.inode.namespaced_ino(entry.inode_nr)
                };

                Ok(buffer.dir_entry(
                    &entry.name,
                    inode_nr,
                    entry.offset as u64,
                    entry.file_type as u32,
                ))
            }
        })?;

        if entry_count > 0 && buffer.is_empty() {
            return Err(lx::Error::EINVAL);
        }

        Ok(buffer)
    }
}

impl SyntheticRootDirFile {
    /// Enumerate the synthetic root of an aggregate virtio-fs.
    ///
    /// Emits `.`, `..`, then one entry per aggregate child. Each child is
    /// presented as a directory of type `DT_DIR`. For READDIRPLUS each child
    /// goes through `lookup_helper`, which routes via
    /// `VirtioFsInode::lookup_child` → `SyntheticRoot::lookup_child` and
    /// registers the resulting submount-root inode in the `InodeMap` with a
    /// valid FUSE node id (so subsequent ops against that node id resolve).
    fn read_dir(
        &self,
        fs: &super::VirtioFs,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        // Snapshot the child names under the AggregateState read lock and
        // release before any FUSE work. Append-only ordering guarantees that
        // any child added between the snapshot and the next READDIR call will
        // simply appear in the next snapshot.
        let names = self
            .inode
            .aggregate_state()
            .expect("file was opened on synthetic root")
            .snapshot_names();

        let mut buffer: Vec<u8> = Vec::with_capacity(size as usize);

        // Build the entry list: index 0 = ".", 1 = "..", 2.. = children.
        // The kernel sends the offset of the *last* entry it accepted; the
        // next read starts at offset+1. Emit entries whose own offset is
        // strictly greater than the requested offset.
        let total_entries = 2 + names.len() as u64;

        for entry_offset in offset..total_entries {
            let next_offset = entry_offset + 1;
            let dir_type = lx::DT_DIR as u32;

            // Compute name and per-entry dirent ino. The dirent ino is a
            // userspace-visible hint only; FUSE routing is by node id, which
            // for READDIRPLUS comes from the `lookup_helper` call below.
            // Per-child synthetic ino values are stable across calls.
            let (name_bytes, dirent_ino): (&[u8], u64) = match entry_offset {
                0 => (b".", FUSE_ROOT_ID),
                1 => (b"..", FUSE_ROOT_ID),
                i => {
                    let idx = (i - 2) as usize;
                    // Use the synthetic offset itself as the dirent ino so
                    // values are stable and unique per entry.
                    (names[idx].as_bytes(), entry_offset)
                }
            };
            let name = lx::LxStr::from_bytes(name_bytes);

            if plus {
                let fuse_entry = if entry_offset < 2 {
                    // For READDIRPLUS, "." and ".." are emitted with a zero
                    // nodeid (left from new_zeroed). This is the standard FUSE
                    // convention telling the kernel not to instantiate/cache an
                    // inode for these entries (no matching FORGET is owed); the
                    // attributes are advisory only. The root is never forgotten,
                    // so its self/parent links don't need real node ids here.
                    let mut e = fuse_entry_out::new_zeroed();
                    e.attr.ino = FUSE_ROOT_ID;
                    e.attr.mode = lx::S_IFDIR | 0o555;
                    e
                } else {
                    if !buffer.check_dir_entry_plus(name) {
                        break;
                    }
                    // Look up the child by name. This registers the
                    // submount-root inode in the InodeMap with a valid
                    // node id and returns the correct fuse_entry_out
                    // (including FUSE_ATTR_SUBMOUNT in the attr).
                    match fs.lookup_helper(&self.inode, name) {
                        Ok(e) => e,
                        Err(e) if e.value() == lx::EACCES || e.value() == lx::ENOENT => continue,
                        Err(e) => return Err(e),
                    }
                };

                if !buffer.dir_entry_plus(name, next_offset, fuse_entry) {
                    break;
                }
            } else if !buffer.dir_entry(name, dirent_ino, next_offset, dir_type) {
                break;
            }
        }

        Ok(buffer)
    }
}
