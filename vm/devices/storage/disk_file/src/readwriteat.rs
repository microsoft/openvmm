// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Helpers for doing IO at a given offset.

use std::fs;
use std::io::Result;

/// A unified extension trait for [`std::fs::File`] for reading/writing at a
/// given offset.
///
/// The semantics are slightly different between Windows and Unix--on Windows,
/// each operation updates the current file pointer, whereas on Unix it does
/// not.
pub trait ReadWriteAt {
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize>;
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;
}

#[cfg(windows)]
impl ReadWriteAt for fs::File {
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        std::os::windows::fs::FileExt::seek_write(self, buf, offset)
    }
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }
}

#[cfg(unix)]
impl ReadWriteAt for fs::File {
    fn write_at(&self, buf: &[u8], offset: u64) -> Result<usize> {
        std::os::unix::fs::FileExt::write_at(self, buf, offset)
    }

    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        std::os::unix::fs::FileExt::read_at(self, buf, offset)
    }
}
