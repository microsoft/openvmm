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

    /// The inode backing this open file.
    pub fn inode(&self) -> &VirtioFsInode {
        &self.inode
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
        self.read_dir_with(offset, size, plus, |name| {
            fs.lookup_helper(&self.inode, name)
        })
    }

    /// Reads directory contents, calling `lookup` to look up each child of a
    /// readdirplus reply. A successful `lookup` takes the lookup reference
    /// that the child's entry hands to the guest.
    fn read_dir_with(
        &self,
        offset: u64,
        size: u32,
        plus: bool,
        mut lookup: impl FnMut(&lx::LxStr) -> lx::Result<fuse_entry_out>,
    ) -> lx::Result<Vec<u8>> {
        let mut file = self.file.write();
        let check_sandbox = !plus && self.inode.volume.is_sandbox();
        let check_children = check_sandbox && !self.inode.volume.supports_stable_file_id();
        if check_sandbox && !check_children {
            // Revalidate the directory and its ancestors once per request.
            // Child filtering uses the host enumeration's metadata snapshot.
            self.inode.check_directory_handle(&file)?;
        }
        let mut buffer = Vec::with_capacity(size as usize);
        let mut entry_count: u32 = 0;
        // Report the directory's guest-visible inode number so `.`/`..` agree
        // with the number reported by lookup/getattr.
        let self_inode_nr = self.inode.guest_inode_nr();
        let result = file.read_dir(offset as lx::off_t, |entry| {
            entry_count += 1;
            // If readdirplus is being used, do a lookup on all items except the . and .. entries.
            if plus {
                let fuse_entry = if entry.name == "." || entry.name == ".." {
                    fuse_entry_out::new_dot(self_inode_nr, (entry.file_type as u32) << 12)
                } else {
                    if !buffer.check_dir_entry_plus(&entry.name) {
                        return Ok(false);
                    }

                    match lookup(&entry.name) {
                        Ok(e) => e,
                        Err(err) => {
                            // Ignore entries that are inaccessible to the
                            // user or deleted. ENOENT can occur if a file was
                            // deleted between enumeration and lookup.
                            if err.value() == lx::EACCES || err.value() == lx::ENOENT {
                                entry_count -= 1;
                                return Ok(true);
                            }
                            return Err(err);
                        }
                    }
                };

                Ok(buffer.dir_entry_plus(&entry.name, entry.offset as u64, fuse_entry))
            } else {
                // Plain readdir does not take FUSE lookup references.
                //
                // Use the current file's inode number for . and .. entries.
                // On Windows inode_nr is 0 for these; on Linux it may be
                // non-zero, so check by name rather than relying on the
                // inode number to identify them.
                let inode_nr = if entry.name == "." || entry.name == ".." {
                    self_inode_nr
                } else {
                    // Without stable IDs, check policy through each child's pathname.
                    // This cannot establish that the parent path still names the
                    // open directory being enumerated.
                    if check_children {
                        match self.inode.lookup_child(&entry.name) {
                            Ok(_) => {}
                            Err(err) if err == lx::Error::EACCES || err == lx::Error::ENOENT => {
                                entry_count -= 1;
                                return Ok(true);
                            }
                            Err(err) => return Err(err),
                        }
                    }
                    // Children share this directory's volume, so apply its
                    // guest inode mapping to match lookup/readdirplus.
                    self.inode.guest_ino(entry.inode_nr)
                };

                Ok(buffer.dir_entry(
                    &entry.name,
                    inode_nr,
                    entry.offset as u64,
                    entry.file_type as u32,
                ))
            }
        });

        // Each readdirplus entry hands the guest a lookup reference, which the
        // guest can release only if it receives the entry. So after an error,
        // return the entries read so far, and report the error only if there
        // are none; the guest's next read starts at the failing entry.
        if let Err(err) = result {
            if buffer.is_empty() {
                return Err(err);
            }
        } else if entry_count > 0 && buffer.is_empty() {
            return Err(lx::Error::EINVAL);
        }

        Ok(buffer)
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        self.file.read().fsync(data_only)
    }

    pub fn fsync_dir(&self, data_only: bool) -> lx::Result<()> {
        self.file.read().fsync_dir(data_only)
    }
}

#[cfg(test)]
mod tests {
    use crate::VirtioFs;
    use crate::test_helpers::decode_dir_entries;
    use fuse::Fuse;
    use fuse::protocol::FUSE_ROOT_ID;
    use std::sync::Arc;
    use test_with_tracing::test;

    #[test]
    fn failed_readdirplus_returns_entries_holding_references() {
        let root = tempfile::tempdir().unwrap();
        for name in ["a", "b"] {
            std::fs::write(root.path().join(name), b"data").unwrap();
        }
        let fs = VirtioFs::new(root.path(), None).unwrap();
        let root_inode = fs.get_inode(FUSE_ROOT_ID).unwrap();
        let dir = Arc::clone(&root_inode).open(lx::O_RDONLY).unwrap();

        // Look up the first child listed, and fail the other with a host error.
        let mut first = None;
        let mut lookup = |name: &lx::LxStr| {
            if first.get_or_insert_with(|| name.as_bytes().to_vec()) == name.as_bytes() {
                fs.lookup_helper(&root_inode, name)
            } else {
                Err(lx::Error::EIO)
            }
        };

        // The read stops at the failing child, but still returns the first
        // child, whose entry holds the reference its lookup took.
        let reply = dir.read_dir_with(0, 4096, true, &mut lookup).unwrap();
        let entries = decode_dir_entries(&reply);
        let children: Vec<_> = entries
            .iter()
            .filter(|entry| entry.name != "." && entry.name != "..")
            .collect();
        assert_eq!(children.len(), 1);
        let node_id = FUSE_ROOT_ID + 1;
        assert_eq!(children[0].node_id, node_id);

        // The next read starts at the failing child, so the guest gets the
        // error.
        let next_offset = entries.last().unwrap().offset;
        assert_eq!(
            dir.read_dir_with(next_offset, 4096, true, &mut lookup)
                .unwrap_err(),
            lx::Error::EIO
        );

        // The guest's forget for the entry it received releases the child.
        fs.forget(node_id, 1);
        assert!(fs.get_inode(node_id).is_err(), "inode leaked");
    }
}
