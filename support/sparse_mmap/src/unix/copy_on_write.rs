// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Copy-on-write file mappings: private mappings whose writes are not
//! carried through to the file.

use super::Mappable;
use super::SparseMapping;
use std::fs::File;
use std::io;
use std::io::Error;
use std::os::unix::prelude::*;

/// Creates a mappable whose writable views are private to the mapping.
pub fn new_mappable_from_file_copy_on_write(
    file: &File,
    _executable: bool,
) -> io::Result<Mappable> {
    file.as_fd().try_clone_to_owned()
}

impl SparseMapping {
    /// Maps a portion of a file privately at `offset`.
    pub fn map_file_copy_on_write(
        &self,
        offset: usize,
        len: usize,
        file_mapping: impl AsFd,
        file_offset: u64,
        writable: bool,
    ) -> Result<(), Error> {
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        // SAFETY: The flags passed in are guaranteed to be valid.
        unsafe {
            self.mmap(
                offset,
                len,
                prot,
                libc::MAP_PRIVATE,
                file_mapping.as_fd(),
                file_offset as i64,
            )
        }
    }
}
