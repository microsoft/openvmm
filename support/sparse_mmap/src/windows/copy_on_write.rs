// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Copy-on-write file mappings: sections whose writable views are private
//! copies of the file pages.

use super::Mappable;
use super::SparseMapping;
use std::io;
use std::io::Error;
use std::os::windows::prelude::*;
use std::ptr::null;
use std::ptr::null_mut;
use windows_sys::Win32::System::Memory::CreateFileMappingW;
use windows_sys::Win32::System::Memory::PAGE_READONLY;
use windows_sys::Win32::System::Memory::PAGE_WRITECOPY;

/// Creates a section whose writable views are private copies of file pages.
pub fn new_mappable_from_file_copy_on_write(file: &std::fs::File) -> io::Result<Mappable> {
    unsafe {
        let section = CreateFileMappingW(
            file.as_raw_handle(),
            null_mut(),
            PAGE_WRITECOPY,
            0,
            0,
            null(),
        ) as RawHandle;
        if section.is_null() {
            return Err(Error::last_os_error());
        }
        Ok(OwnedHandle::from_raw_handle(section))
    }
}

impl SparseMapping {
    /// Maps a portion of a file as a private copy-on-write view.
    ///
    /// Windows cannot unmap part of a view, so a writable view can only be
    /// unmapped or mapped over as a whole: doing either to part of it fails
    /// with [`io::ErrorKind::Unsupported`] instead of discarding the view's
    /// private pages.
    pub fn map_file_copy_on_write(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsHandle,
        file_offset: u64,
        writable: bool,
    ) -> Result<(), Error> {
        let protect = if writable {
            PAGE_WRITECOPY
        } else {
            PAGE_READONLY
        };
        self.map_view_of_file(
            offset,
            len,
            file_mapping.as_handle(),
            file_offset,
            protect,
            None,
        )
    }
}
