// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::fs;
use super::macros::impl_directory_information;
use super::util;
use crate::windows::path;
use arrayvec::ArrayVec;
use bitfield_struct::bitfield;
use pal::windows::UnicodeString;
use pal::windows::UnicodeStringRef;
use std::ffi;
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::os::windows::io::OwnedHandle;
use std::ptr;
use windows::Wdk::Storage::FileSystem;
use windows::Wdk::Storage::FileSystem::FILE_INFORMATION_CLASS;
use windows::Wdk::System::Threading;
use windows::Win32::Foundation;
use windows::Win32::Storage::FileSystem as W32Fs;
use windows::Win32::System::Kernel;
use windows::Win32::System::Memory;
use windows::Win32::System::Threading as W32Threading;

const DIR_ENUM_BUFFER_SIZE: usize = 4096;
const BUFFER_EXTRA_SIZE: usize = 0x200;

/// Maximum entries to cache per window.
///
/// The Linux kernel rounds up getdents buffer requests to 4096 bytes. With
/// typical directory entry sizes, this allows approximately 32 entries per
/// kernel request. By caching 64 entries (2x the kernel (due to a cache miss
/// buffer), we reduce the number of repeated guest filesystem enumeration calls
/// that would be needed when the guest requests small buffers while deleting
/// entries.
///
/// The cache provides a sliding window of directory entries with stable
/// offsets.  This ensures stable enumeration even when files are deleted
/// between calls: offsets remain valid within the cache window, so the guest
/// won't skip or repeat entries due to host-side directory changes.
///
/// Note: when serving entries from the cache, we return only the entries
/// currently cached and then stop (rather than partially serving the cache and
/// immediately refilling to serve more). By stopping at the cache boundary, the
/// next call naturally starts a fresh window at the boundary offset.
const CACHE_MAX_ENTRIES: usize = 64;

#[expect(non_snake_case)]
#[repr(C)]
pub struct FILE_ID_64_EXTD_DIR_INFORMATION {
    pub NextEntryOffset: u32,
    pub FileIndex: u32,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub EndOfFile: i64,
    pub AllocationSize: i64,
    pub FileAttributes: u32,
    pub FileNameLength: u32,
    pub EaSize: u32,
    pub ReparsePointTag: u32,
    pub FileId: i64,
    pub FileName: [u16; 1],
}

#[expect(non_snake_case)]
#[repr(C)]
pub struct FILE_ID_ALL_EXTD_DIR_INFORMATION {
    pub NextEntryOffset: u32,
    pub FileIndex: u32,
    pub CreationTime: i64,
    pub LastAccessTime: i64,
    pub LastWriteTime: i64,
    pub ChangeTime: i64,
    pub EndOfFile: i64,
    pub AllocationSize: i64,
    pub FileAttributes: u32,
    pub FileNameLength: u32,
    pub EaSize: u32,
    pub ReparsePointTag: u32,
    pub FileId: i64,
    pub FileId128: W32Fs::FILE_ID_128,
    pub FileName: [u16; 1],
}

#[bitfield(u32)]
struct DirectoryEnumeratorFlags {
    end_reached: bool,
    asynchronous_mode: bool,
    #[bits(30)]
    _reserved: u32,
}

// Some of these FileInformationClasses are missing from windows-rs.
#[expect(clippy::enum_variant_names)]
#[derive(PartialEq)]
enum DirectoryEnumeratorFileInformationClass {
    FileId64ExtdDirectoryInformation,
    FileIdAllExtdDirectoryInformation,
    FileIdExtdDirectoryInformation,
    FileIdFullDirectoryInformation,
    FileIdBothDirectoryInformation,
    FileFullDirectoryInformation,
    FileDirectoryInformation,
}

trait DirectoryInformation {
    fn file_id(&self) -> i64;
    fn file_name(&self) -> lx::Result<UnicodeStringRef<'_>>;
    fn file_attributes(&self) -> u32;
    fn reparse_tag(&self) -> u32;
}

// Implement DirectoryInformation for the structures which all have similar implementations.
impl_directory_information!(
    FILE_ID_64_EXTD_DIR_INFORMATION, FileAttributes, ReparsePointTag;
    FILE_ID_ALL_EXTD_DIR_INFORMATION, FileAttributes, ReparsePointTag;
    FileSystem::FILE_ID_FULL_DIR_INFORMATION, FileAttributes, EaSize;
    FileSystem::FILE_ID_BOTH_DIR_INFORMATION, FileAttributes, EaSize;
);

impl DirectoryInformation for FileSystem::FILE_ID_EXTD_DIR_INFORMATION {
    // Take the first 8 bytes of the i64 as the file ID.
    fn file_id(&self) -> i64 {
        i64::from_ne_bytes(*self.FileId.Identifier.first_chunk::<8>().unwrap())
    }

    fn file_name(&self) -> Result<UnicodeStringRef<'_>, lx::Error> {
        // SAFETY: A properly constructed struct will contain the name in a buffer at the end.
        let name_slice = unsafe {
            std::slice::from_raw_parts(
                self.FileName.as_ptr(),
                self.FileNameLength as usize / size_of::<u16>(),
            )
        };
        UnicodeStringRef::new(name_slice).ok_or(lx::Error::EINVAL)
    }

    fn file_attributes(&self) -> u32 {
        self.FileAttributes
    }

    fn reparse_tag(&self) -> u32 {
        self.ReparsePointTag
    }
}

impl DirectoryInformation for FileSystem::FILE_FULL_DIR_INFORMATION {
    fn file_id(&self) -> i64 {
        0
    }

    fn file_name(&self) -> Result<UnicodeStringRef<'_>, lx::Error> {
        // SAFETY: A properly constructed struct will contain the name in a buffer at the end.
        let name_slice = unsafe {
            std::slice::from_raw_parts(
                self.FileName.as_ptr(),
                self.FileNameLength as usize / size_of::<u16>(),
            )
        };
        UnicodeStringRef::new(name_slice).ok_or(lx::Error::EINVAL)
    }

    fn file_attributes(&self) -> u32 {
        self.FileAttributes
    }

    fn reparse_tag(&self) -> u32 {
        self.EaSize
    }
}

impl DirectoryInformation for FileSystem::FILE_DIRECTORY_INFORMATION {
    fn file_id(&self) -> i64 {
        0
    }

    fn file_name(&self) -> Result<UnicodeStringRef<'_>, lx::Error> {
        // SAFETY: A properly constructed struct will contain the name in a buffer at the end.
        let name_slice = unsafe {
            std::slice::from_raw_parts(
                self.FileName.as_ptr(),
                self.FileNameLength as usize / size_of::<u16>(),
            )
        };
        UnicodeStringRef::new(name_slice).ok_or(lx::Error::EINVAL)
    }

    fn file_attributes(&self) -> u32 {
        self.FileAttributes
    }

    fn reparse_tag(&self) -> u32 {
        0
    }
}

/// A cached directory entry with a stable offset that survives file deletions.
#[derive(Clone, Debug, PartialEq)]
struct CachedDirEntry {
    offset: u64,
    inode_nr: u64,
    name: lx::LxString,
    file_type: u8,
}

/// Information about a directory entry read from the filesystem.
#[derive(Clone)]
struct DirEntryInfo {
    pub inode_nr: u64,
    pub name: lx::LxString,
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

/// A cursor for iterating directory entries with stable offsets.
struct DirEntryCursor {
    entries: ArrayVec<CachedDirEntry, CACHE_MAX_ENTRIES>,
    window_start: u64,
    host_consumed: u64,
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
        // Cache is valid if offset is within [window_start, last_entry.offset)
        offset >= self.window_start && offset < self.entries.last().map_or(0, |e| e.offset)
    }

    /// Find the index of the first entry to serve for the given offset.
    /// Returns the index of the first entry with offset > given offset.
    fn find_start_index(&self, offset: u64) -> usize {
        assert!(offset >= self.window_start);
        ((offset - self.window_start) as usize).min(self.entries.len())
    }
    
    /// Get entries starting from the given offset.
    fn entries_from(&self, offset: u64) -> &[CachedDirEntry] {
        let start = self.find_start_index(offset);
        &self.entries[start..]
    }

    /// Check if we need more entries (at end of window but not complete).
    fn needs_more(&self, offset: u64) -> bool {
        !self.complete && self.entries.last().is_none_or(|e| e.offset <= offset)
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
                inode_nr: entry.inode_nr,
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

/// A DirectoryEnumerator that owns its buffer.
pub struct DirectoryEnumerator {
    buffer: *mut ffi::c_void,
    buffer_next_entry: *mut ffi::c_void,
    buffer_size: u32,
    next_read_index: u32,
    file_information_class: DirectoryEnumeratorFileInformationClass,
    flags: DirectoryEnumeratorFlags,
    cursor: DirEntryCursor,
}

pub struct FileDirectoryInformation {
    file_id: i64,
    file_name: UnicodeString,
    file_attributes: u32,
    reparse_tag: u32,
}

unsafe impl Send for DirectoryEnumerator {}

unsafe impl Sync for DirectoryEnumerator {}

impl DirectoryEnumerator {
    /// Create a new DirectoryEnumerator that owns its buffer. The buffer will be
    /// freed on drop.
    pub fn new(asynchronous_mode: bool) -> lx::Result<Self> {
        let buf = unsafe {
            FileSystem::RtlAllocateHeap(
                Memory::GetProcessHeap().map_err(|_| lx::Error::ENOMEM)?.0,
                None,
                DIR_ENUM_BUFFER_SIZE,
            )
        };
        assert!(!buf.is_null(), "out of memory");
        Ok(Self {
            buffer: buf,
            buffer_next_entry: ptr::null_mut(),
            buffer_size: DIR_ENUM_BUFFER_SIZE as _,
            flags: DirectoryEnumeratorFlags::new().with_asynchronous_mode(asynchronous_mode),
            next_read_index: 0,
            file_information_class:
                DirectoryEnumeratorFileInformationClass::FileId64ExtdDirectoryInformation,
            cursor: DirEntryCursor::new(),
        })
    }

    /// Read the contents of the directory and write out the results using
    /// a custom write function.
    ///
    /// Uses a sliding window cache to ensure stable enumeration even when files
    /// are deleted between calls. Offsets remain stable within the cache window.
    pub fn read_dir<F>(
        &mut self,
        handle: &OwnedHandle,
        fs_context: &fs::FsContext,
        offset: &mut lx::off_t,
        callback: &mut F,
    ) -> lx::Result<()>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        let requested_offset = *offset as u64;

        // Ensure cache is populated for the requested offset.
        self.ensure_cache_populated(handle, fs_context, requested_offset)?;

        // Serve entries from cache.
        loop {
            let entries = self.cursor.entries_from(requested_offset);

            if entries.is_empty() && self.cursor.needs_more(requested_offset) {
                // Need to fetch more entries - refill the cache.
                self.refill_cache(handle, fs_context, requested_offset)?;
                continue;
            }

            // Serve cached entries to the callback.
            for entry in entries.iter() {
                let dir_entry = lx::DirEntry {
                    name: entry.name.clone(),
                    inode_nr: entry.inode_nr,
                    offset: (entry.offset as lx::off_t) + super::DOT_ENTRY_COUNT,
                    file_type: entry.file_type,
                };

                let result = callback(dir_entry)?;
                if result {
                    // Update offset to this entry's offset (which points to the next entry).
                    *offset = entry.offset as lx::off_t;
                } else {
                    // User wants to stop.
                    return Ok(());
                }
            }
            break;
        }

        Ok(())
    }

    /// Ensure the cache is populated for the given offset.
    fn ensure_cache_populated(
        &mut self,
        handle: &OwnedHandle,
        fs_context: &fs::FsContext,
        offset: u64,
    ) -> lx::Result<()> {
        // Check if cache is valid.
        if offset != 0 && self.cursor.contains(offset) {
            return Ok(());
        }

        // Determine if this is a sequential continuation.
        // Backward seeks are never sequential (require full refresh).
        let is_sequential = offset != 0
            && !self.cursor.entries.is_empty()
            && offset == self.cursor.entries.last().map_or(0, |e| e.offset);

        if offset == 0 {
            self.cursor.reset();
            // Also reset the Windows enumerator state.
            self.next_read_index = 0;
            self.buffer_next_entry = ptr::null_mut();
            self.flags.set_end_reached(false);
        }

        let mut source = WindowsDirEntrySource {
            enumerator: self,
            handle,
            fs_context,
        };

        // Save cursor state and populate.
        let mut cursor = std::mem::replace(&mut source.enumerator.cursor, DirEntryCursor::new());
        let result = cursor.populate(offset, &mut source, is_sequential);
        source.enumerator.cursor = cursor;
        result
    }

    /// Refill cache when at window boundary.
    fn refill_cache(
        &mut self,
        handle: &OwnedHandle,
        fs_context: &fs::FsContext,
        offset: u64,
    ) -> lx::Result<()> {
        // Re-check.
        if !self.cursor.needs_more(offset) {
            return Ok(());
        }

        let mut source = WindowsDirEntrySource {
            enumerator: self,
            handle,
            fs_context,
        };

        // Save cursor state and populate (always sequential when refilling).
        let mut cursor = std::mem::replace(&mut source.enumerator.cursor, DirEntryCursor::new());
        let result = cursor.populate(offset, &mut source, true);
        source.enumerator.cursor = cursor;
        result
    }

    /// Get the current entry from the enumerator. If there are no more entries, this function
    /// returns None.
    fn read_current(
        &mut self,
        handle: &OwnedHandle,
        restart_scan: bool,
    ) -> lx::Result<Option<FileDirectoryInformation>> {
        if restart_scan {
            self.buffer_next_entry = ptr::null_mut();
            self.flags.set_end_reached(false);
        }

        // If the end was previously reached, avoid calling ZwQueryDirectoryFile
        // again. This is done since there will be an extra call to getdents after
        // the last call that returns entries. Since it was already determined
        // there are no more entries on the previous call, calling again is not
        // necessary.
        if self.flags.end_reached() {
            return Ok(None);
        }

        if self.buffer_next_entry.is_null() {
            let bytes_read = self.fill_buffer(handle, restart_scan)?;

            if bytes_read == 0 {
                debug_assert!(self.buffer_next_entry.is_null());

                self.flags.set_end_reached(true);
                return Ok(None);
            }
        }

        debug_assert!(!self.buffer_next_entry.is_null());
        let entry: &dyn DirectoryInformation = match self.file_information_class {
            DirectoryEnumeratorFileInformationClass::FileId64ExtdDirectoryInformation => {
                self.get_next_entry::<FILE_ID_64_EXTD_DIR_INFORMATION>()?
            }
            DirectoryEnumeratorFileInformationClass::FileIdAllExtdDirectoryInformation => {
                self.get_next_entry::<FILE_ID_ALL_EXTD_DIR_INFORMATION>()?
            }
            DirectoryEnumeratorFileInformationClass::FileIdExtdDirectoryInformation => {
                self.get_next_entry::<FileSystem::FILE_ID_EXTD_DIR_INFORMATION>()?
            }
            DirectoryEnumeratorFileInformationClass::FileIdFullDirectoryInformation => {
                self.get_next_entry::<FileSystem::FILE_ID_FULL_DIR_INFORMATION>()?
            }
            DirectoryEnumeratorFileInformationClass::FileIdBothDirectoryInformation => {
                self.get_next_entry::<FileSystem::FILE_ID_BOTH_DIR_INFORMATION>()?
            }
            DirectoryEnumeratorFileInformationClass::FileFullDirectoryInformation => {
                self.get_next_entry::<FileSystem::FILE_FULL_DIR_INFORMATION>()?
            }
            DirectoryEnumeratorFileInformationClass::FileDirectoryInformation => {
                self.get_next_entry::<FileSystem::FILE_DIRECTORY_INFORMATION>()?
            }
        };

        let file_name =
            UnicodeString::new(entry.file_name()?.as_slice()).map_err(|_| lx::Error::EIO)?;

        let file_info = FileDirectoryInformation {
            file_id: entry.file_id(),
            file_name,
            file_attributes: entry.file_attributes(),
            reparse_tag: entry.reparse_tag(),
        };

        Ok(Some(file_info))
    }

    /// Fills the buffer of the enumerator. Returns the number of bytes read into the buffer.
    fn fill_buffer(&mut self, handle: &OwnedHandle, restart_scan: bool) -> lx::Result<u32> {
        debug_assert!(self.buffer_next_entry.is_null());

        let mut raw_event = Default::default();
        let _event;
        // SAFETY: Calling Win32 API as documented.
        if self.flags.asynchronous_mode() {
            unsafe {
                let _ = util::check_status(FileSystem::NtCreateEvent(
                    &mut raw_event,
                    W32Threading::EVENT_ALL_ACCESS.0,
                    None,
                    Kernel::SynchronizationEvent,
                    false,
                ))?;
                _event = OwnedHandle::from_raw_handle(raw_event.0);
            }
        }

        let mut iosb = Default::default();
        loop {
            // SAFETY: Calling Win32 API as documented.
            let mut status = unsafe {
                FileSystem::NtQueryDirectoryFile(
                    Foundation::HANDLE(handle.as_raw_handle()),
                    if raw_event.is_invalid() {
                        None
                    } else {
                        Some(raw_event)
                    },
                    None,
                    None,
                    &mut iosb,
                    self.buffer,
                    self.buffer_size,
                    self.current_file_information_class(),
                    false,
                    None,
                    restart_scan,
                )
            };

            if status == Foundation::STATUS_PENDING {
                // SAFETY: Calling Win32 API as documented.
                if unsafe { Threading::NtWaitForSingleObject(raw_event, false, ptr::null_mut()) }
                    != Foundation::STATUS_SUCCESS
                {
                    return Err(lx::Error::EINVAL);
                }

                // SAFETY: Accessing field of correctly constructed union.
                status = unsafe { iosb.Anonymous.Status };
            }

            let mut buffer_too_small = true;
            if status.0 < 0 {
                match status {
                    Foundation::STATUS_BUFFER_OVERFLOW => {
                        // If the buffer was too small, ignore the last, incomplete entry.
                        // N.B. The buffer is treated as a FILE_DIRECTORY_INFORMATION
                        //      even if it's actually a different structure. All
                        //      the fields used are at the same offset in all structs.
                        let mut offset = 0;
                        let mut prev_offset = 0;

                        // Loop through all of the complete entries.
                        while let Ok(entry) =
                            self.get_buffer::<FileSystem::FILE_DIRECTORY_INFORMATION>(offset)
                        {
                            prev_offset = offset;
                            offset += entry.NextEntryOffset as usize;
                            buffer_too_small = false;
                        }

                        // Set the final complete entry's next offset to 0.
                        if let Ok(previous) =
                            self.get_buffer::<FileSystem::FILE_DIRECTORY_INFORMATION>(prev_offset)
                        {
                            previous.NextEntryOffset = 0;
                        }
                    }
                    Foundation::STATUS_NO_MORE_FILES | Foundation::STATUS_NO_SUCH_FILE => {
                        //
                        // Some file systems (or filter drivers) may not support the
                        // current information class. In that case, fall back to a
                        // different information class.
                        //
                        // N.B. In this case, the inode number or reparse tag reported
                        //      to the caller may be zero.
                        //
                        return Ok(0);
                    }
                    Foundation::STATUS_NOT_SUPPORTED
                    | Foundation::STATUS_INVALID_PARAMETER
                    | Foundation::STATUS_INVALID_INFO_CLASS => {
                        if !restart_scan
                            || self.file_information_class
                                == DirectoryEnumeratorFileInformationClass::FileDirectoryInformation
                        {
                            return Err(lx::Error::ENOTSUP);
                        } else {
                            self.file_information_class = match self.file_information_class {
                                // Use the next FILE_INFORMATION_CLASS
                                DirectoryEnumeratorFileInformationClass::FileId64ExtdDirectoryInformation =>
                                    DirectoryEnumeratorFileInformationClass::FileIdAllExtdDirectoryInformation,
                                DirectoryEnumeratorFileInformationClass::FileIdAllExtdDirectoryInformation =>
                                    DirectoryEnumeratorFileInformationClass::FileIdExtdDirectoryInformation,
                                DirectoryEnumeratorFileInformationClass::FileIdExtdDirectoryInformation =>
                                    DirectoryEnumeratorFileInformationClass::FileIdFullDirectoryInformation,
                                DirectoryEnumeratorFileInformationClass::FileIdFullDirectoryInformation =>
                                    DirectoryEnumeratorFileInformationClass::FileIdBothDirectoryInformation,
                                DirectoryEnumeratorFileInformationClass::FileIdBothDirectoryInformation =>
                                    DirectoryEnumeratorFileInformationClass::FileFullDirectoryInformation,
                                DirectoryEnumeratorFileInformationClass::FileFullDirectoryInformation =>
                                    DirectoryEnumeratorFileInformationClass::FileDirectoryInformation,
                                DirectoryEnumeratorFileInformationClass::FileDirectoryInformation => {
                                    return Err(lx::Error::ENOTSUP);
                                },
                            }
                        }
                    }
                    _ => return Err(util::nt_status_to_lx(status)),
                }
            } else {
                buffer_too_small = false;
            }

            // On the first call, ZwQueryDirectoryFile returns buffer overflow if
            // the buffer can't hold at least one entry. On subsequent calls, it
            // returns success but the number of bytes read is zero.
            if !buffer_too_small && iosb.Information != 0 {
                self.buffer_next_entry = self.buffer;
                break;
            }

            // Try to grow the buffer. The size is guaranteed not to overflow as
            // buffer_size is u32 and usize::MAX > u32::MAX.
            self.free_buffer();
            let buf = unsafe {
                FileSystem::RtlAllocateHeap(
                    Memory::GetProcessHeap().map_err(|_| lx::Error::ENOMEM)?.0,
                    None,
                    self.buffer_size as usize + BUFFER_EXTRA_SIZE,
                )
            };
            assert!(!buf.is_null(), "out of memory");
            self.buffer = buf;
        }
        Ok(iosb.Information as _)
    }

    fn get_buffer<T>(&mut self, offset: usize) -> lx::Result<&mut T>
    where
        T: DirectoryInformation,
    {
        if self.buffer.is_null() || offset + size_of::<T>() > self.buffer_size as _ {
            return Err(lx::Error::EFAULT);
        }

        // The buffer is guaranteed to be large enough for this operation.
        let ptr = self.buffer.wrapping_byte_add(offset as _);

        if ptr.align_offset(align_of::<T>()) != 0 {
            return Err(lx::Error::EFAULT);
        }

        // SAFETY: The pointer is aligned and will read within the buffer bounds.
        unsafe { Ok(&mut *(ptr.cast())) }
    }

    fn get_next_entry<T>(&mut self) -> lx::Result<&mut T>
    where
        T: DirectoryInformation,
    {
        if self.buffer_next_entry.is_null()
            || self.buffer_next_entry.align_offset(align_of::<T>()) != 0
        {
            return Err(lx::Error::EFAULT);
        }

        // SAFETY: The pointer is aligned and will read within the buffer bounds.
        unsafe { Ok(&mut *(self.buffer_next_entry.cast())) }
    }

    /// Advances the enumerator to the next entry.
    fn next(&mut self) -> lx::Result<()> {
        debug_assert!(!self.buffer.is_null() && !self.buffer_next_entry.is_null());

        // If the end was previously reached, do nothing.
        if self.flags.end_reached() {
            return Ok(());
        }

        let entry: FileSystem::FILE_FULL_DIR_INFORMATION = self.get_next_entry().cloned()?;
        if entry.NextEntryOffset != 0 {
            self.buffer_next_entry = self
                .buffer_next_entry
                .wrapping_byte_add(entry.NextEntryOffset as _);
        } else {
            self.buffer_next_entry = ptr::null_mut();
        }

        Ok(())
    }

    fn current_file_information_class(&self) -> FILE_INFORMATION_CLASS {
        match self.file_information_class {
            // The first two are missing from windows-rs.
            DirectoryEnumeratorFileInformationClass::FileId64ExtdDirectoryInformation => {
                FILE_INFORMATION_CLASS(78)
            }
            DirectoryEnumeratorFileInformationClass::FileIdAllExtdDirectoryInformation => {
                FILE_INFORMATION_CLASS(80)
            }
            DirectoryEnumeratorFileInformationClass::FileIdExtdDirectoryInformation => {
                FileSystem::FileIdExtdDirectoryInformation
            }
            DirectoryEnumeratorFileInformationClass::FileIdFullDirectoryInformation => {
                FileSystem::FileIdFullDirectoryInformation
            }
            DirectoryEnumeratorFileInformationClass::FileIdBothDirectoryInformation => {
                FileSystem::FileIdBothDirectoryInformation
            }
            DirectoryEnumeratorFileInformationClass::FileFullDirectoryInformation => {
                FileSystem::FileFullDirectoryInformation
            }
            DirectoryEnumeratorFileInformationClass::FileDirectoryInformation => {
                FileSystem::FileDirectoryInformation
            }
        }
    }

    /// Free the buffer with RtlFreeHeap.
    fn free_buffer(&self) {
        // SAFETY: Calling Win32 API as documented.
        unsafe {
            FileSystem::RtlFreeHeap(Memory::GetProcessHeap().unwrap().0, None, Some(self.buffer))
        };
    }
}

impl Drop for DirectoryEnumerator {
    fn drop(&mut self) {
        self.free_buffer();
    }
}

/// Adapter to use DirectoryEnumerator as a DirEntrySource for the cursor.
struct WindowsDirEntrySource<'a> {
    enumerator: &'a mut DirectoryEnumerator,
    handle: &'a OwnedHandle,
    fs_context: &'a fs::FsContext,
}

impl DirEntrySource for WindowsDirEntrySource<'_> {
    fn read_entries<F>(&mut self, offset: u64, mut callback: F) -> lx::Result<()>
    where
        F: FnMut(DirEntryInfo) -> lx::Result<bool>,
    {
        if offset == 0 {
            self.enumerator.next_read_index = 0;
        }

        // Skip entries until we reach the requested offset.
        while (self.enumerator.next_read_index as u64) < offset {
            match self
                .enumerator
                .read_current(self.handle, self.enumerator.next_read_index == 0)?
            {
                Some(file_info) => {
                    // Skip . and .. entries.
                    if !util::is_self_relative_unicode_path(&file_info.file_name) {
                        self.enumerator.next_read_index += 1;
                    }
                    self.enumerator.next()?;
                }
                None => {
                    return Ok(());
                }
            }
        }

        let mut did_first_read = false;
        loop {
            // Only restart scan on the very first read of the main loop
            let should_restart = self.enumerator.next_read_index == 0 && !did_first_read;
            did_first_read = true;
            match self.enumerator.read_current(self.handle, should_restart)? {
                Some(file_info) => {
                    // Skip . and .. entries - advance to next but don't increment next_read_index
                    // since . and .. don't count towards our index
                    if util::is_self_relative_unicode_path(&file_info.file_name) {
                        self.enumerator.next()?;
                        continue;
                    }

                    self.enumerator.next_read_index += 1;

                    // Determine the file type.
                    let entry_type = if !self.fs_context.compatibility_flags.server_reparse_points()
                        && file_info.file_attributes & W32Fs::FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
                    {
                        util::reparse_tag_to_file_type(file_info.reparse_tag)
                    } else if file_info.file_attributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                        lx::DT_DIR
                    } else {
                        lx::DT_REG
                    };

                    // Unescape the LX path.
                    let file_name = path::unescape_path(file_info.file_name.as_slice())?;

                    let entry_info = DirEntryInfo {
                        inode_nr: file_info.file_id as u64,
                        name: file_name,
                        file_type: entry_type,
                    };

                    if !callback(entry_info)? {
                        break;
                    }

                    self.enumerator.next()?;
                }
                None => {
                    break;
                }
            }
        }

        Ok(())
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
                inode_nr: 999, // This should be preserved (unlike in file.rs where it was zeroed)
                name: ".".into(),
                file_type: lx::DT_DIR,
            },
            DirEntryInfo {
                inode_nr: 888,
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

        // Inode numbers are preserved as-is (the caller handles special cases)
        assert_eq!(cursor.entries[0].inode_nr, 999);
        assert_eq!(cursor.entries[1].inode_nr, 888);
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
