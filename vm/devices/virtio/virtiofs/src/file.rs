// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::inode::VirtioFsInode;
use crate::util;
use fuse::DirEntryWriter;
use fuse::protocol::fuse_attr;
use fuse::protocol::fuse_entry_out;
use fuse::protocol::fuse_setattr_in;
use fuse::protocol::fuse_statx;
use lxutil::LxFile;
use parking_lot::RwLock;
use std::sync::Arc;
use zerocopy::FromZeros;

/// Implements file callbacks for virtio-fs.
pub struct VirtioFsFile {
    file: RwLock<LxFile>,
    inode: Arc<VirtioFsInode>,
}

impl VirtioFsFile {
    /// Create a new file.
    pub fn new(file: LxFile, inode: Arc<VirtioFsInode>) -> Self {
        Self {
            file: RwLock::new(file),
            inode,
        }
    }

    /// Gets the attributes of the open file.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.file.read().fstat()?.into();
        Ok(util::stat_to_fuse_attr(&stat))
    }

    /// Gets the statx details for the open file.
    pub fn get_statx(&self) -> lx::Result<fuse_statx> {
        let statx = self.file.read().fstat()?;
        Ok(util::statx_to_fuse_statx(&statx))
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
        let mut buffer = Vec::with_capacity(size as usize);
        let mut entry_count: u32 = 0;
        let self_inode_nr = self.inode.inode_nr();
        let mut file = self.file.write();
        file.read_dir(offset as lx::off_t, |entry| {
            entry_count += 1;
            let get_child_fuse_entry = || -> lx::Result<Option<fuse_entry_out>> {
                match fs.lookup_helper(&self.inode, &entry.name) {
                    Ok(e) => Ok(Some(e)),
                    Err(err) => {
                        // Ignore entries that are inaccessible to the user.
                        if err.value() == lx::EACCES {
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
                // Windows doesn't report the inode number for . and .., so just use the current file's
                // inode number for that.
                let inode_nr = if entry.inode_nr == 0 {
                    self_inode_nr
                } else {
                    if get_child_fuse_entry()?.is_none() {
                        // Ignore entries that are inaccessible to the user.
                        entry_count -= 1;
                        return Ok(true);
                    }
                    entry.inode_nr
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

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        self.file.read().fsync(data_only)
    }
}
