// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flushing modified pages of shared file mappings to their files.

use super::SparseMapping;
use std::io::Error;

impl SparseMapping {
    /// Flushes modified shared file pages in a populated range.
    ///
    /// `offset` and `len` must be multiples of [`Self::page_size`], and the
    /// range must lie within the mapping; otherwise, this fails with
    /// [`io::ErrorKind::InvalidInput`](std::io::ErrorKind::InvalidInput). An
    /// empty range is a no-op.
    pub fn flush(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        // `msync` rejects an empty range on some platforms, such as macOS.
        if len == 0 {
            return Ok(());
        }
        // SAFETY: `validate_offset_len` proves the range is page-aligned, as
        // `msync` requires, and within this reservation. Callers use this only
        // for populated shared mappings.
        if unsafe { libc::msync(self.address.add(offset), len, libc::MS_SYNC) } < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }
}
