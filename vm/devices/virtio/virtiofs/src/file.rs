// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::inode::VirtioFsInode;
use crate::util;
use fuse::DirEntryWriter;
use fuse::protocol::fuse_attr;
use fuse::protocol::fuse_entry_out;
use fuse::protocol::fuse_setattr_in;
use lxutil::LxFile;
use parking_lot::RwLock;
use std::sync::Arc;
use zerocopy::FromZeros;

/// A cached directory entry with a stable offset that survives file deletions.
#[derive(Clone)]
struct CachedDirEntry {
    /// The stable offset assigned to this entry (the offset to pass to get the NEXT entry).
    offset: u64,
    /// The inode number (0 for `.` and `..` entries, resolved at serve time).
    inode_nr: u64,
    /// The file name.
    name: lx::LxString,
    /// The file type (e.g., DT_REG, DT_DIR).
    file_type: u8,
}

/// Target cache size in bytes (matches Linux kernel's internal FUSE buffer).
const CACHE_TARGET_BYTES: usize = 4096;

/// A cursor for iterating directory entries with stable offsets.
///
/// This provides stable directory enumeration even when files are deleted during
/// enumeration. The Linux kernel's FUSE client internally buffers ~4KB of directory
/// entries, and this cursor maintains a matching cache with stable offsets.
struct DirEntryCursor {
    /// Cached entries with stable offsets.
    entries: Vec<CachedDirEntry>,
    /// The offset that starts this cache window (the seek offset used to populate it).
    window_start: u64,
    /// Number of entries consumed from host filesystem (for resuming enumeration).
    host_consumed: u64,
    /// Approximate size in bytes of cached entries.
    cached_bytes: usize,
    /// Whether we've reached the end of the directory.
    complete: bool,
}

impl DirEntryCursor {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            window_start: 0,
            host_consumed: 0,
            cached_bytes: 0,
            complete: false,
        }
    }

    /// Reset the cursor to the beginning.
    fn reset(&mut self) {
        self.entries.clear();
        self.window_start = 0;
        self.host_consumed = 0;
        self.cached_bytes = 0;
        self.complete = false;
    }

    /// Check if the cache contains entries for the given offset.
    fn contains(&self, offset: u64) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        if offset == 0 {
            return self.window_start == 0;
        }
        // Cache is valid if offset is within [window_start, last_entry.offset]
        offset >= self.window_start && offset < self.entries.last().map_or(0, |e| e.offset + 1)
    }

    /// Find the index of the first entry to serve for the given offset.
    /// Returns the index of the first entry with offset > given offset.
    fn find_start_index(&self, offset: u64) -> usize {
        self.entries
            .binary_search_by(|e| e.offset.cmp(&offset))
            .map_or_else(|idx| idx, |idx| idx + 1)
    }

    /// Get entries starting from the given offset.
    fn entries_from(&self, offset: u64) -> &[CachedDirEntry] {
        let start = self.find_start_index(offset);
        &self.entries[start..]
    }

    /// Check if we need more entries (at end of window but not complete).
    fn needs_more(&self, offset: u64) -> bool {
        !self.complete && self.find_start_index(offset) >= self.entries.len()
    }

    /// Populate the cache window starting from the given offset.
    ///
    /// If `sequential` is true, we're continuing from the end of the current window.
    /// Otherwise, we need to restart enumeration and skip to the target offset.
    fn populate(&mut self, offset: u64, file: &mut LxFile, sequential: bool) -> lx::Result<()> {
        // Reset window state (but keep host_consumed if sequential).
        self.entries.clear();
        self.cached_bytes = 0;
        self.complete = false;
        self.window_start = offset;

        if !sequential {
            // Random seek - must restart from beginning.
            self.host_consumed = 0;
        }

        let start_host_count = self.host_consumed;
        let mut next_offset = offset + 1;
        let skip_until = if sequential { 0 } else { offset };
        let mut skipping = !sequential && offset != 0;
        let mut batch_consumed = 0u64;

        file.read_dir(start_host_count as lx::off_t, |entry| {
            batch_consumed += 1;

            // Skip entries until we reach the target offset (for random seeks).
            if skipping {
                if next_offset <= skip_until {
                    next_offset += 1;
                    return Ok(true);
                }
                skipping = false;
            }

            // Cache this entry.
            self.entries.push(CachedDirEntry {
                offset: next_offset,
                inode_nr: if entry.name == "." || entry.name == ".." {
                    0 // Resolved at serve time
                } else {
                    entry.inode_nr
                },
                name: entry.name.clone(),
                file_type: entry.file_type,
            });

            self.cached_bytes += Self::estimate_entry_size(&entry.name);
            next_offset += 1;

            // Stop if we've cached enough bytes.
            if self.cached_bytes >= CACHE_TARGET_BYTES {
                return Ok(false);
            }

            Ok(true)
        })?;

        self.host_consumed += batch_consumed;
        self.complete = self.cached_bytes < CACHE_TARGET_BYTES;

        Ok(())
    }

    /// Estimate the size in bytes of a directory entry for READDIRPLUS.
    fn estimate_entry_size(name: &lx::LxString) -> usize {
        // fuse_direntplus: fuse_entry_out (128) + fuse_dirent (24) + name + padding
        128 + 24 + name.len() + 8
    }
}

/// Implements file callbacks for virtio-fs.
pub struct VirtioFsFile {
    file: RwLock<LxFile>,
    inode: Arc<VirtioFsInode>,
    /// Cursor for directory enumeration with stable offsets.
    dir_cursor: RwLock<DirEntryCursor>,
}

impl VirtioFsFile {
    /// Create a new file.
    pub fn new(file: LxFile, inode: Arc<VirtioFsInode>) -> Self {
        Self {
            file: RwLock::new(file),
            inode,
            dir_cursor: RwLock::new(DirEntryCursor::new()),
        }
    }

    /// Gets the attributes of the open file.
    pub fn get_attr(&self) -> lx::Result<fuse_attr> {
        let stat = self.file.read().fstat()?;
        Ok(util::stat_to_fuse_attr(&stat))
    }

    /// Sets the attributes of the open file.
    pub fn set_attr(&self, arg: &fuse_setattr_in, request_uid: lx::uid_t) -> lx::Result<()> {
        let attr = util::fuse_set_attr_to_lxutil(arg, request_uid);
        self.file.read().set_attr(attr)
    }

    /// Read data from the file.
    pub fn read(&self, buffer: &mut [u8], offset: u64) -> lx::Result<usize> {
        self.file.read().pread(buffer, offset as lx::off_t)
    }

    /// Write data to the file.
    pub fn write(&self, buffer: &[u8], offset: u64, thread_uid: lx::uid_t) -> lx::Result<usize> {
        self.file
            .read()
            .pwrite(buffer, offset as lx::off_t, thread_uid)
    }

    /// Read directory contents with stable offsets.
    ///
    /// Uses a sliding window cache to ensure stable enumeration even when files
    /// are deleted between calls. Offsets remain stable within the ~4KB cache window.
    pub fn read_dir(
        &self,
        fs: &super::VirtioFs,
        offset: u64,
        size: u32,
        plus: bool,
    ) -> lx::Result<Vec<u8>> {
        let mut buffer = Vec::with_capacity(size as usize);
        let self_inode = self.inode.inode_nr();

        // Ensure cache is populated for the requested offset.
        self.ensure_cache_populated(offset)?;

        // Serve entries from cache, refilling if needed.
        loop {
            let cursor = self.dir_cursor.read();
            let entries = cursor.entries_from(offset);

            if entries.is_empty() && cursor.needs_more(offset) {
                // Need to fetch more entries - drop read lock and refill.
                drop(cursor);
                self.refill_cache(offset)?;
                continue;
            }

            // Serve cached entries to the buffer.
            for entry in entries {
                if !self.write_dir_entry(&mut buffer, fs, entry, self_inode, plus)? {
                    break; // Buffer full
                }
            }
            break;
        }

        Ok(buffer)
    }

    /// Ensure the cache is populated for the given offset.
    fn ensure_cache_populated(&self, offset: u64) -> lx::Result<()> {
        // Check if cache already contains the offset.
        if offset != 0 && self.dir_cursor.read().contains(offset) {
            return Ok(());
        }

        let mut cursor = self.dir_cursor.write();

        // Double-check under write lock.
        if offset != 0 && cursor.contains(offset) {
            return Ok(());
        }

        // Determine if this is a sequential continuation or random seek.
        let is_sequential = offset != 0
            && !cursor.entries.is_empty()
            && offset == cursor.entries.last().map_or(0, |e| e.offset);

        if offset == 0 {
            cursor.reset();
        }

        let mut file = self.file.write();
        cursor.populate(offset, &mut file, is_sequential)
    }

    /// Refill cache when at window boundary.
    fn refill_cache(&self, offset: u64) -> lx::Result<()> {
        let mut cursor = self.dir_cursor.write();

        // Re-check under write lock.
        if !cursor.needs_more(offset) {
            return Ok(());
        }

        let mut file = self.file.write();
        cursor.populate(offset, &mut file, true) // Always sequential when refilling
    }

    /// Write a single directory entry to the buffer.
    ///
    /// Returns `true` if the entry was written, `false` if the buffer is full.
    fn write_dir_entry(
        &self,
        buffer: &mut Vec<u8>,
        fs: &super::VirtioFs,
        entry: &CachedDirEntry,
        self_inode: u64,
        plus: bool,
    ) -> lx::Result<bool> {
        let is_dot_entry = entry.name == "." || entry.name == "..";

        // Lookup child entry (skip for . and ..).
        let lookup_result = if is_dot_entry {
            None
        } else {
            match fs.lookup_helper(&self.inode, &entry.name) {
                Ok(e) => Some(e),
                Err(err) if err.value() == lx::EACCES || err.value() == lx::ENOENT => {
                    // Entry deleted or inaccessible - skip it.
                    return Ok(true);
                }
                Err(err) => return Err(err),
            }
        };

        if plus {
            // READDIRPLUS: include full entry_out.
            let fuse_entry = if is_dot_entry {
                let mut e = fuse_entry_out::new_zeroed();
                e.attr.ino = self_inode;
                e.attr.mode = (entry.file_type as u32) << 12;
                e
            } else {
                // Check buffer space before lookup (which adds inode ref).
                if !buffer.check_dir_entry_plus(&entry.name) {
                    return Ok(false);
                }
                lookup_result.unwrap()
            };

            Ok(buffer.dir_entry_plus(&entry.name, entry.offset, fuse_entry))
        } else {
            // READDIR: just inode and type.
            let inode_nr = if is_dot_entry {
                self_inode
            } else {
                entry.inode_nr
            };

            Ok(buffer.dir_entry(&entry.name, inode_nr, entry.offset, entry.file_type as u32))
        }
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        self.file.read().fsync(data_only)
    }
}
