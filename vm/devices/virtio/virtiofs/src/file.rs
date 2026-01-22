// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::inode::VirtioFsInode;
use crate::util;
use arrayvec::ArrayVec;
use fuse::DirEntryWriter;
use fuse::protocol::fuse_attr;
use fuse::protocol::fuse_entry_out;
use fuse::protocol::fuse_setattr_in;
use fuse::protocol::fuse_statx;
use lxutil::LxFile;
use parking_lot::RwLock;
use std::sync::Arc;
use zerocopy::FromZeros;

/// Information about a directory entry, used by [`DirEntrySource`].
#[derive(Clone)]
struct DirEntryInfo {
    /// The inode number.
    pub inode_nr: u64,
    /// The file name.
    pub name: lx::LxString,
    /// The file type (e.g., DT_REG, DT_DIR).
    pub file_type: u8,
}

/// Trait for reading directory entries, enabling mock implementations for testing.
trait DirEntrySource {
    /// Read directory entries starting at the given offset.
    ///
    /// Calls `callback` for each entry. If callback returns `Ok(true)`, continue
    /// reading. If it returns `Ok(false)`, stop reading.
    fn read_entries<F>(&mut self, offset: u64, callback: F) -> lx::Result<()>
    where
        F: FnMut(DirEntryInfo) -> lx::Result<bool>;
}

impl DirEntrySource for LxFile {
    fn read_entries<F>(&mut self, offset: u64, mut callback: F) -> lx::Result<()>
    where
        F: FnMut(DirEntryInfo) -> lx::Result<bool>,
    {
        self.read_dir(offset as lx::off_t, |entry| {
            callback(DirEntryInfo {
                inode_nr: entry.inode_nr,
                name: entry.name.clone(),
                file_type: entry.file_type,
            })
        })
    }
}

/// A cached directory entry with a stable offset that survives file deletions.
#[derive(Clone, Debug, PartialEq)]
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

/// Maximum entries to cache per window (4096/sizeof(fuse_entry_out))
/// 4096 is the size linux kernel uses for readdir buffer.
const CACHE_MAX_ENTRIES: usize = 32;

/// Result of attempting to write a directory entry to the buffer.
enum WriteResult {
    /// Entry was successfully written to the buffer.
    Written,
    /// Entry was skipped (e.g., inaccessible or deleted).
    Skipped,
    /// Buffer is full, entry was not written.
    BufferFull,
}

/// A cursor for iterating directory entries with stable offsets.
struct DirEntryCursor {
    /// Cached entries with stable offsets.
    entries: ArrayVec<CachedDirEntry, CACHE_MAX_ENTRIES>,
    /// The offset that starts this cache window (the seek offset used to populate it).
    window_start: u64,
    /// Number of entries consumed from host filesystem (for resuming enumeration).
    host_consumed: u64,
    /// Whether we've reached the end of the directory.
    complete: bool,
}

impl DirEntryCursor {
    fn new() -> Self {
        Self {
            entries: ArrayVec::new(),
            window_start: 0,
            host_consumed: 0,
            complete: false,
        }
    }

    /// Reset the cursor to the beginning.
    fn reset(&mut self) {
        self.entries.clear();
        self.window_start = 0;
        self.host_consumed = 0;
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
    fn populate(
        &mut self,
        offset: u64,
        source: &mut impl DirEntrySource,
        sequential: bool,
    ) -> lx::Result<()> {
        // Reset window state (but keep host_consumed if sequential).
        self.entries.clear();
        self.complete = false;
        self.window_start = offset;

        if !sequential {
            // Random seek - must restart from beginning.
            self.host_consumed = 0;
        }

        let start_host_count = self.host_consumed;
        let mut next_offset = offset + 1;
        let entries_to_skip = if sequential { 0 } else { offset };
        let mut entries_skipped = 0u64;
        let mut batch_consumed = 0u64;

        source.read_entries(start_host_count, |entry| {
            batch_consumed += 1;

            // Skip entries until we've skipped enough (for random seeks).
            // When seeking to offset N, we need to skip N entries to get to the (N+1)th entry.
            if entries_skipped < entries_to_skip {
                entries_skipped += 1;
                return Ok(true);
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

            next_offset += 1;

            // Stop if we've cached enough entries.
            if self.entries.len() >= CACHE_MAX_ENTRIES {
                return Ok(false);
            }

            Ok(true)
        })?;

        self.host_consumed += batch_consumed;
        self.complete = self.entries.len() < CACHE_MAX_ENTRIES;

        Ok(())
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
        let mut entry_count: u32 = 0;

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
                match self.write_dir_entry(&mut buffer, fs, entry, self_inode, plus)? {
                    WriteResult::Written => {
                        entry_count += 1;
                    }
                    WriteResult::Skipped => {
                        // Just continue to next entry.
                    }
                    WriteResult::BufferFull => {
                        break; // Buffer full
                    }
                }
            }
            break;
        }

        if entry_count > 0 && buffer.is_empty() {
            return Err(lx::Error::EINVAL);
        }

        Ok(buffer)
    }

    /// Ensure the cache is populated for the given offset.
    fn ensure_cache_populated(&self, offset: u64) -> lx::Result<()> {
        let cursor = self.dir_cursor.read();

        // Check if cache is valid and not stale.
        if offset != 0 && cursor.contains(offset) {
            return Ok(());
        }

        drop(cursor);
        let mut cursor = self.dir_cursor.write();

        // Double-check under write lock.
        if offset != 0 && cursor.contains(offset) {
            return Ok(());
        }

        // Determine if this is a sequential continuation.
        // Backward seeks are never sequential (require full refresh).
        let is_sequential = offset != 0
            && !cursor.entries.is_empty()
            && offset == cursor.entries.last().map_or(0, |e| e.offset);

        if offset == 0 {
            cursor.reset();
        }

        let mut file = self.file.write();
        cursor.populate(offset, &mut *file, is_sequential)
    }

    /// Refill cache when at window boundary.
    fn refill_cache(&self, offset: u64) -> lx::Result<()> {
        let mut cursor = self.dir_cursor.write();

        // Re-check under write lock.
        if !cursor.needs_more(offset) {
            return Ok(());
        }

        let mut file = self.file.write();
        cursor.populate(offset, &mut *file, true) // Always sequential when refilling
    }

    /// Write a single directory entry to the buffer.
    ///
    /// Returns the result indicating whether the entry was written, skipped, or buffer was full.
    fn write_dir_entry(
        &self,
        buffer: &mut Vec<u8>,
        fs: &super::VirtioFs,
        entry: &CachedDirEntry,
        self_inode: u64,
        plus: bool,
    ) -> lx::Result<WriteResult> {
        let is_dot_entry = entry.name == "." || entry.name == "..";

        // Helper to lookup child entry and filter inaccessible entries.
        let get_child_fuse_entry = || -> lx::Result<Option<fuse_entry_out>> {
            match fs.lookup_helper(&self.inode, &entry.name) {
                Ok(e) => Ok(Some(e)),
                Err(err) => {
                    // Ignore entries that are inaccessible or deleted.
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
            let fuse_entry = if is_dot_entry {
                let mut e = fuse_entry_out::new_zeroed();
                e.attr.ino = self_inode;
                e.attr.mode = (entry.file_type as u32) << 12;
                e
            } else {
                // Check buffer space before lookup (which adds inode ref).
                if !buffer.check_dir_entry_plus(&entry.name) {
                    return Ok(WriteResult::BufferFull);
                }

                match get_child_fuse_entry()? {
                    Some(e) => e,
                    None => {
                        // Ignore entries that are inaccessible to the user.
                        return Ok(WriteResult::Skipped);
                    }
                }
            };

            if buffer.dir_entry_plus(&entry.name, entry.offset, fuse_entry) {
                Ok(WriteResult::Written)
            } else {
                Ok(WriteResult::BufferFull)
            }
        } else {
            // Windows doesn't report the inode number for . and .., so just use the current
            // file's inode number for that.
            let inode_nr = if entry.inode_nr == 0 {
                self_inode
            } else {
                if get_child_fuse_entry()?.is_none() {
                    // Ignore entries that are inaccessible to the user.
                    return Ok(WriteResult::Skipped);
                }
                entry.inode_nr
            };

            if buffer.dir_entry(&entry.name, inode_nr, entry.offset, entry.file_type as u32) {
                Ok(WriteResult::Written)
            } else {
                Ok(WriteResult::BufferFull)
            }
        }
    }

    pub fn fsync(&self, data_only: bool) -> lx::Result<()> {
        self.file.read().fsync(data_only)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mock directory entry source for testing.
    struct MockDirSource {
        /// All entries in the "directory".
        entries: Vec<DirEntryInfo>,
    }

    impl MockDirSource {
        fn new(entries: Vec<DirEntryInfo>) -> Self {
            Self { entries }
        }

        /// Create a source with N numbered files
        fn with_n_files(n: usize) -> Self {
            let mut entries = vec![
                DirEntryInfo {
                    inode_nr: 0,
                    name: ".".into(),
                    file_type: lx::DT_DIR,
                },
                DirEntryInfo {
                    inode_nr: 0,
                    name: "..".into(),
                    file_type: lx::DT_DIR,
                },
            ];
            for i in 0..n {
                entries.push(DirEntryInfo {
                    inode_nr: 100 + i as u64,
                    name: format!("file_{}", i).into(),
                    file_type: lx::DT_REG,
                });
            }
            Self::new(entries)
        }
    }

    impl DirEntrySource for MockDirSource {
        fn read_entries<F>(&mut self, offset: u64, mut callback: F) -> lx::Result<()>
        where
            F: FnMut(DirEntryInfo) -> lx::Result<bool>,
        {
            for entry in self.entries.iter().skip(offset as usize) {
                if !callback(entry.clone())? {
                    break;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn contains_empty_cache_returns_false() {
        let cursor = DirEntryCursor::new();
        assert!(!cursor.contains(0));
        assert!(!cursor.contains(1));
        assert!(!cursor.contains(100));
    }

    #[test]
    fn contains_offset_zero_with_window_start_zero() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.push(CachedDirEntry {
            offset: 1,
            inode_nr: 0,
            name: ".".into(),
            file_type: lx::DT_DIR,
        });
        cursor.window_start = 0;

        assert!(cursor.contains(0));
    }

    #[test]
    fn contains_offset_zero_with_nonzero_window_start() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.push(CachedDirEntry {
            offset: 11,
            inode_nr: 100,
            name: "file".into(),
            file_type: lx::DT_REG,
        });
        cursor.window_start = 10;

        // Offset 0 should not be contained when window_start != 0
        assert!(!cursor.contains(0));
    }

    #[test]
    fn contains_offset_within_window() {
        let mut cursor = DirEntryCursor::new();
        cursor.window_start = 5;
        cursor.entries.extend([
            CachedDirEntry {
                offset: 6,
                inode_nr: 100,
                name: "a".into(),
                file_type: lx::DT_REG,
            },
            CachedDirEntry {
                offset: 7,
                inode_nr: 101,
                name: "b".into(),
                file_type: lx::DT_REG,
            },
            CachedDirEntry {
                offset: 8,
                inode_nr: 102,
                name: "c".into(),
                file_type: lx::DT_REG,
            },
        ]);

        // window_start (5) is valid - can serve entry with offset 6
        assert!(cursor.contains(5));
        // Offsets within window are valid
        assert!(cursor.contains(6));
        assert!(cursor.contains(7));
        // Last entry offset (8) is still valid since we have an entry with offset 8
        // (contains checks if we can serve entries starting from this offset)
        assert!(cursor.contains(8));
        // Outside window - before window_start
        assert!(!cursor.contains(4));
        // Outside window - after last entry offset
        assert!(!cursor.contains(9));
        assert!(!cursor.contains(100));
    }

    #[test]
    fn find_start_index_empty() {
        let cursor = DirEntryCursor::new();
        assert_eq!(cursor.find_start_index(0), 0);
        assert_eq!(cursor.find_start_index(10), 0);
    }

    #[test]
    fn find_start_index_returns_first_entry_greater_than_offset() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.extend([
            CachedDirEntry {
                offset: 1,
                inode_nr: 0,
                name: ".".into(),
                file_type: lx::DT_DIR,
            },
            CachedDirEntry {
                offset: 2,
                inode_nr: 0,
                name: "..".into(),
                file_type: lx::DT_DIR,
            },
            CachedDirEntry {
                offset: 3,
                inode_nr: 100,
                name: "file".into(),
                file_type: lx::DT_REG,
            },
        ]);

        // Offset 0: first entry with offset > 0 is index 0 (offset=1)
        assert_eq!(cursor.find_start_index(0), 0);
        // Offset 1: first entry with offset > 1 is index 1 (offset=2)
        assert_eq!(cursor.find_start_index(1), 1);
        // Offset 2: first entry with offset > 2 is index 2 (offset=3)
        assert_eq!(cursor.find_start_index(2), 2);
        // Offset 3: no entry with offset > 3
        assert_eq!(cursor.find_start_index(3), 3);
        // Offset beyond all entries
        assert_eq!(cursor.find_start_index(100), 3);
    }

    #[test]
    fn entries_from_returns_all_for_offset_zero() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.extend([
            CachedDirEntry {
                offset: 1,
                inode_nr: 0,
                name: ".".into(),
                file_type: lx::DT_DIR,
            },
            CachedDirEntry {
                offset: 2,
                inode_nr: 100,
                name: "file".into(),
                file_type: lx::DT_REG,
            },
        ]);

        let entries = cursor.entries_from(0);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, lx::LxString::from("."));
        assert_eq!(entries[1].name, lx::LxString::from("file"));
    }

    #[test]
    fn entries_from_returns_subset() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.extend([
            CachedDirEntry {
                offset: 1,
                inode_nr: 0,
                name: ".".into(),
                file_type: lx::DT_DIR,
            },
            CachedDirEntry {
                offset: 2,
                inode_nr: 0,
                name: "..".into(),
                file_type: lx::DT_DIR,
            },
            CachedDirEntry {
                offset: 3,
                inode_nr: 100,
                name: "file".into(),
                file_type: lx::DT_REG,
            },
        ]);

        let entries = cursor.entries_from(1);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, lx::LxString::from(".."));

        let entries = cursor.entries_from(2);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, lx::LxString::from("file"));
    }

    #[test]
    fn entries_from_returns_empty_past_end() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.push(CachedDirEntry {
            offset: 1,
            inode_nr: 100,
            name: "file".into(),
            file_type: lx::DT_REG,
        });

        assert!(cursor.entries_from(1).is_empty());
        assert!(cursor.entries_from(100).is_empty());
    }

    #[test]
    fn needs_more_returns_false_when_complete() {
        let mut cursor = DirEntryCursor::new();
        cursor.complete = true;
        cursor.entries.push(CachedDirEntry {
            offset: 1,
            inode_nr: 100,
            name: "file".into(),
            file_type: lx::DT_REG,
        });

        assert!(!cursor.needs_more(0));
        assert!(!cursor.needs_more(1));
        assert!(!cursor.needs_more(100));
    }

    #[test]
    fn needs_more_returns_true_at_window_boundary() {
        let mut cursor = DirEntryCursor::new();
        cursor.complete = false;
        cursor.entries.push(CachedDirEntry {
            offset: 1,
            inode_nr: 100,
            name: "file".into(),
            file_type: lx::DT_REG,
        });

        // At offset 1, we're past all cached entries
        assert!(cursor.needs_more(1));
    }

    #[test]
    fn needs_more_returns_false_when_entries_available() {
        let mut cursor = DirEntryCursor::new();
        cursor.complete = false;
        cursor.entries.extend([
            CachedDirEntry {
                offset: 1,
                inode_nr: 100,
                name: "a".into(),
                file_type: lx::DT_REG,
            },
            CachedDirEntry {
                offset: 2,
                inode_nr: 101,
                name: "b".into(),
                file_type: lx::DT_REG,
            },
        ]);

        // At offset 0, we have entries to serve
        assert!(!cursor.needs_more(0));
        // At offset 1, we still have entry at index 1
        assert!(!cursor.needs_more(1));
        // At offset 2, we're past all entries
        assert!(cursor.needs_more(2));
    }

    #[test]
    fn reset_clears_all_state() {
        let mut cursor = DirEntryCursor::new();
        cursor.entries.push(CachedDirEntry {
            offset: 1,
            inode_nr: 100,
            name: "file".into(),
            file_type: lx::DT_REG,
        });
        cursor.window_start = 10;
        cursor.host_consumed = 50;
        cursor.complete = true;

        cursor.reset();

        assert!(cursor.entries.is_empty());
        assert_eq!(cursor.window_start, 0);
        assert_eq!(cursor.host_consumed, 0);
        assert!(!cursor.complete);
    }

    #[test]
    fn populate_caches_entries_from_source() {
        let mut cursor = DirEntryCursor::new();
        let mut source = MockDirSource::with_n_files(3);

        cursor.populate(0, &mut source, false).unwrap();

        // Should have . + .. + 3 files = 5 entries
        assert_eq!(cursor.entries.len(), 5);
        assert_eq!(cursor.entries[0].name, lx::LxString::from("."));
        assert_eq!(cursor.entries[0].offset, 1);
        assert_eq!(cursor.entries[0].inode_nr, 0); // dot entry
        assert_eq!(cursor.entries[1].name, lx::LxString::from(".."));
        assert_eq!(cursor.entries[1].offset, 2);
        assert_eq!(cursor.entries[2].name, lx::LxString::from("file_0"));
        assert_eq!(cursor.entries[2].offset, 3);
        assert_eq!(cursor.entries[2].inode_nr, 100);
    }

    #[test]
    fn populate_respects_max_entries() {
        let mut cursor = DirEntryCursor::new();
        // Create more files than CACHE_MAX_ENTRIES
        let mut source = MockDirSource::with_n_files(CACHE_MAX_ENTRIES + 10);

        cursor.populate(0, &mut source, false).unwrap();

        assert_eq!(cursor.entries.len(), CACHE_MAX_ENTRIES);
        assert!(!cursor.complete); // More entries available
    }

    #[test]
    fn populate_sets_complete_when_fewer_entries() {
        let mut cursor = DirEntryCursor::new();
        let mut source = MockDirSource::with_n_files(3);

        cursor.populate(0, &mut source, false).unwrap();

        assert!(cursor.complete);
    }

    #[test]
    fn populate_sequential_continues_from_host_consumed() {
        let mut cursor = DirEntryCursor::new();
        let mut source = MockDirSource::with_n_files(CACHE_MAX_ENTRIES + 5);

        // First populate
        cursor.populate(0, &mut source, false).unwrap();
        assert_eq!(cursor.entries.len(), CACHE_MAX_ENTRIES);
        let last_offset = cursor.entries.last().unwrap().offset;

        // Sequential populate - should continue from where we left off
        cursor.populate(last_offset, &mut source, true).unwrap();

        // Should have the remaining entries (5 files after the first 32-2=30 files)
        // Total entries: 2 (dot) + CACHE_MAX_ENTRIES + 5 = 39
        // First batch: 32, remaining: 7
        assert_eq!(cursor.entries.len(), 7);
        assert!(cursor.complete);
    }

    #[test]
    fn populate_random_seek_resets_host_consumed() {
        let mut cursor = DirEntryCursor::new();
        let mut source = MockDirSource::with_n_files(10);

        // First populate
        cursor.populate(0, &mut source, false).unwrap();
        assert_eq!(cursor.host_consumed, 12); // 2 dot entries + 10 files

        // Random seek to offset 5 (non-sequential)
        cursor.populate(5, &mut source, false).unwrap();

        // Should have re-read from beginning and skipped 5 entries
        // Entries 6-12 should be cached (7 entries)
        assert_eq!(cursor.entries.len(), 7);
        assert_eq!(cursor.entries[0].offset, 6);
        assert!(cursor.complete);
    }

    #[test]
    fn populate_handles_dot_entries_inode() {
        let mut cursor = DirEntryCursor::new();
        let mut source = MockDirSource::new(vec![
            DirEntryInfo {
                inode_nr: 999, // This should be ignored
                name: ".".into(),
                file_type: lx::DT_DIR,
            },
            DirEntryInfo {
                inode_nr: 888, // This should be ignored
                name: "..".into(),
                file_type: lx::DT_DIR,
            },
            DirEntryInfo {
                inode_nr: 100,
                name: "file".into(),
                file_type: lx::DT_REG,
            },
        ]);

        cursor.populate(0, &mut source, false).unwrap();

        // Dot entries should have inode_nr = 0 (resolved at serve time)
        assert_eq!(cursor.entries[0].inode_nr, 0);
        assert_eq!(cursor.entries[1].inode_nr, 0);
        // Regular file keeps its inode
        assert_eq!(cursor.entries[2].inode_nr, 100);
    }

    #[test]
    fn populate_updates_window_start() {
        let mut cursor = DirEntryCursor::new();
        let mut source = MockDirSource::with_n_files(5);

        cursor.populate(0, &mut source, false).unwrap();
        assert_eq!(cursor.window_start, 0);

        cursor.populate(3, &mut source, false).unwrap();
        assert_eq!(cursor.window_start, 3);
    }
}
