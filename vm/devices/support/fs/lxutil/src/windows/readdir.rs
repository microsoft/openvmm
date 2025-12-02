// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::fs;
use super::macros::impl_directory_information;
use super::util;
use crate::windows::path;
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

/// A DirectoryEnumerator that owns its buffer.
pub struct DirectoryEnumerator {
    buffer: *mut ffi::c_void,
    buffer_next_entry: *mut ffi::c_void,
    buffer_size: u32,
    next_read_index: u32,
    file_information_class: DirectoryEnumeratorFileInformationClass,
    flags: DirectoryEnumeratorFlags,
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
        })
    }

    /// Read the contents of the directory and write out the results using
    /// a custom write function.
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
        let mut restart_scan = if (*offset as u32) < self.next_read_index {
            self.next_read_index = 0;
            true
        } else {
            // If this is the first read, restart_scan should be true.
            self.next_read_index == 0
        };

        // Loop over all the entries in the enumerator.
        while let Some(file_info) = self.read_current(handle, restart_scan)? {
            restart_scan = false;

            // Ignore . and .. entries returned by Windows.
            if util::is_self_relative_unicode_path(&file_info.file_name) {
                self.next()?;
                continue;
            }

            // Loop until the desired index of directory entry is reached.
            self.next_read_index += 1;
            if *offset >= self.next_read_index as _ {
                self.next()?;
                continue;
            }

            // Determine the file type.
            //
            // N.B. For reparse points other than the specific types used for
            //      special files it's assumed the file's metadata is correct.
            //
            // N.B. On SMB file systems, all reparse points (symlink, junction,
            //      or otherwise) are handled by the server. While they show up
            //      in the directory information as reparse points, they should
            //      not be treated as such, so don't report them as symlinks to
            //      Linux.
            let entry_type = if !fs_context.compatibility_flags.server_reparse_points()
                && file_info.file_attributes & W32Fs::FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
            {
                util::reparse_tag_to_file_type(file_info.reparse_tag)
            } else {
                if file_info.file_attributes & W32Fs::FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                    lx::DT_DIR
                } else {
                    lx::DT_REG
                }
            };

            // Unescape the LX path.
            let file_name = path::unescape_path(file_info.file_name.as_slice())?;

            let result = self.process_dir_entry(
                callback,
                offset,
                file_info.file_id,
                &file_name,
                entry_type,
            )?;

            if result {
                // THe closure directed to continue. The offset has been advanced by process_dir_entry.
                self.next()?;
            } else {
                // The closure directed to stop. The entry was not written, so revert the next read index
                // and do not advance the enumerator.
                debug_assert!(self.next_read_index > 0);

                self.next_read_index -= 1;
                break;
            }
        }
        Ok(())
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

    /// Process a dir entry using a user-provided callback. Returns whether the user wants to continue.
    fn process_dir_entry<F>(
        &self,
        callback: &mut F,
        offset: &mut lx::off_t,
        file_id: i64,
        name: &lx::LxString,
        entry_type: u8,
    ) -> lx::Result<bool>
    where
        F: FnMut(lx::DirEntry) -> lx::Result<bool>,
    {
        let entry = lx::DirEntry {
            name: name.clone(),
            inode_nr: file_id as _,
            offset: *offset + 1 + super::DOT_ENTRY_COUNT, // Pass the offset of the next entry plus the number of dot entries processed.
            file_type: entry_type,
        };

        let result = (callback)(entry)?;

        // Update the offset only if the user wants to continue.
        if result {
            *offset += 1;
        }

        Ok(result)
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
