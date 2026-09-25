// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Flushing modified pages of shared file mappings to their files.

use super::SparseMapping;
use std::io::Error;

impl SparseMapping {
    /// Flushes modified shared file pages in a populated range.
    pub fn flush(&self, offset: usize, len: usize) -> Result<(), Error> {
        let _ = self.validate_offset_len(offset, len)?;
        // SAFETY: `validate_offset_len` proves the range is within this
        // reservation. Callers use this only for populated shared mappings.
        if unsafe { libc::msync(self.address.add(offset), len, libc::MS_SYNC) } < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }
}
