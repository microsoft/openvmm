// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::inode::VirtioFsInode;
use crate::microvm::saved_state::SavedDirectoryEntry;
use crate::util;
use fuse::DirEntryWriter;
use fuse::protocol::fuse_attr;
use fuse::protocol::fuse_entry_out;
use fuse::protocol::fuse_setattr_in;
use fuse::protocol::fuse_statx;
use lxutil::LxFile;
use parking_lot::RwLock;
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct DirectorySnapshot {
    pub(crate) built: bool,
    pub(crate) entries: Vec<SavedDirectoryEntry>,
}

/// Implements file callbacks for virtio-fs.
pub struct VirtioFsFile {
    pub(crate) file: RwLock<LxFile>,
    pub(crate) inode: Arc<VirtioFsInode>,
    open_flags: u32,
    pub(crate) directory_snapshot: RwLock<DirectorySnapshot>,
}

impl VirtioFsFile {
    /// Create a new file.
    pub fn new(file: LxFile, inode: Arc<VirtioFsInode>, open_flags: u32) -> Self {
        Self {
            file: RwLock::new(file),
            inode,
            open_flags,
            directory_snapshot: RwLock::new(DirectorySnapshot::default()),
        }
    }

    /// The inode backing this open file.
    pub fn inode(&self) -> &VirtioFsInode {
        &self.inode
    }

    pub(crate) fn open_flags(&self) -> u32 {
        self.open_flags
    }

    pub(crate) fn directory_entries(&self) -> Vec<SavedDirectoryEntry> {
        self.directory_snapshot.read().entries.clone()
    }

    pub(crate) fn directory_snapshot_built(&self) -> bool {
        self.directory_snapshot.read().built
    }

    pub(crate) fn object_stat(&self) -> lx::Result<lx::Stat> {
        self.file.read().fstat().map(Into::into)
    }

    pub(crate) fn restore_directory_snapshot(
        &self,
        built: bool,
        entries: Vec<SavedDirectoryEntry>,
    ) -> lx::Result<()> {
        Self::validate_directory_entries(&entries)?;
        if !built && !entries.is_empty() {
            return Err(lx::Error::EINVAL);
        }
        if built {
            let stat = self.object_stat()?;
            if stat.mode & lx::S_IFMT != lx::S_IFDIR {
                return Err(lx::Error::ENOTDIR);
            }
        }
        *self.directory_snapshot.write() = DirectorySnapshot { built, entries };
        Ok(())
    }

    /// Gets the attributes of the open file.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.file.read().fstat()?.into();
        Ok(self.inode.attr_from_stat(&stat))
    }

    /// Gets the statx details for the open file.
    pub fn get_statx(&self) -> lx::Result<fuse_statx> {
        let statx = self.file.read().fstat()?;
        Ok(self.inode.statx_from(&statx))
    }

    /// Sets the attributes of the open file.
    pub fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<()> {
        let attr = util::fuse_set_attr_to_lxutil(arg, request_uid);

        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared
        // depending on the attributes being set. Lxutil takes care of that on Windows (and Linux
        // does it naturally).
        self.file.read().set_attr(attr)
    }

    /// Read data from the file.
    pub fn read(&self, buffer: &mut [u8], offset: u64) -> lx::Result<usize> {
        self.file.read().pread(buffer, offset as lx::off_t)
    }

    /// Write data to the file.
    pub fn write(&self, buffer: &[u8], offset: u64, thread_uid: lx::uid_t) -> lx::Result<usize> {
        // Because FUSE_HANDLE_KILLPRIV is set, set-user-ID and set-group-ID must be cleared on
        // write. Lxutil takes care of that on Windows (and Linux does it naturally).
        self.file
            .read()
            .pwrite(buffer, offset as lx::off_t, thread_uid)
    }

    /// Read directory contents.
    pub fn read_dir(
        &self,
        fs: &super::VirtioFs,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        if fs.is_microvm() {
            return self.read_dir_microvm(fs, offset, size, plus);
        }
        if size as usize > crate::MAX_GUEST_BUFFER_SIZE {
            return Err(lx::Error::E2BIG);
        }
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(size as usize)
            .map_err(|_| lx::Error::ENOMEM)?;
        let mut entry_count: u32 = 0;
        // Report the directory's guest-visible inode number so `.`/`..` agree
        // with the number reported by lookup/getattr.
        let self_inode_nr = self.inode.guest_inode_nr();
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
                    fuse_entry_out::new_dot(self_inode_nr, (entry.file_type as u32) << 12)
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

                let written = buffer.dir_entry_plus(&entry.name, entry.offset as u64, fuse_entry);
                Ok(written)
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
                    // Children share this directory's volume, so apply its
                    // guest inode mapping to match lookup/readdirplus.
                    self.inode.guest_ino(entry.inode_nr)
                };

                let written = buffer.dir_entry(
                    &entry.name,
                    inode_nr,
                    entry.offset as u64,
                    entry.file_type as u32,
                );
                Ok(written)
            }
        })?;

        if entry_count > 0 && buffer.is_empty() {
            return Err(lx::Error::EINVAL);
        }

        Ok(buffer)
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        self.file.read().fsync(data_only)
    }
}
