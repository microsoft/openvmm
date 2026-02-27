// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Memory-related abstractions.

// UNSAFETY: Manual pointer manipulation, dealing with mmap, and a signal handler.
#![expect(unsafe_code)]
#![expect(missing_docs)]
#![expect(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

pub mod alloc;
pub mod unix;
pub mod windows;

pub use sys::AsMappableRef;
pub use sys::Mappable;
pub use sys::MappableRef;
pub use sys::SparseMapping;
pub use sys::alloc_shared_memory;
pub use sys::new_mappable_from_file;

use std::mem::MaybeUninit;
use std::sync::atomic::AtomicU8;
use thiserror::Error;
#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

#[derive(Debug, Error)]
pub enum SparseMappingError {
    #[error("out of bounds")]
    OutOfBounds,
    #[error(transparent)]
    Memory(trycopy::MemoryError),
}

impl SparseMapping {
    /// Gets the supported page size for sparse mappings.
    pub fn page_size() -> usize {
        sys::page_size()
    }

    fn check(&self, offset: usize, len: usize) -> Result<(), SparseMappingError> {
        if self.len() < offset || self.len() - offset < len {
            return Err(SparseMappingError::OutOfBounds);
        }
        Ok(())
    }

    /// Reads a type `T` from `offset` in the sparse mapping using a single read instruction.
    ///
    /// Panics if `T` is not 1, 2, 4, or 8 bytes in size.
    pub fn read_volatile<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
    ) -> Result<T, SparseMappingError> {
        assert!(self.is_local(), "cannot read from remote mappings");

        self.check(offset, size_of::<T>())?;
        // SAFETY: the bounds have been checked above.
        unsafe { trycopy::try_read_volatile(self.as_ptr().byte_add(offset).cast()) }
            .map_err(SparseMappingError::Memory)
    }

    /// Writes a type `T` at `offset` in the sparse mapping using a single write instruciton.
    ///
    /// Panics if `T` is not 1, 2, 4, or 8 bytes in size.
    pub fn write_volatile<T: IntoBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
        value: &T,
    ) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot write to remote mappings");

        self.check(offset, size_of::<T>())?;
        // SAFETY: the bounds have been checked above.
        unsafe { trycopy::try_write_volatile(self.as_ptr().byte_add(offset).cast(), value) }
            .map_err(SparseMappingError::Memory)
    }

    /// Tries to write into the sparse mapping.
    pub fn write_at(&self, offset: usize, data: &[u8]) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot write to remote mappings");

        self.check(offset, data.len())?;
        // SAFETY: the bounds have been checked above.
        unsafe {
            let dest = self.as_ptr().cast::<u8>().add(offset);
            trycopy::try_copy(data.as_ptr(), dest, data.len()).map_err(SparseMappingError::Memory)
        }
    }

    /// Tries to read from the sparse mapping.
    pub fn read_at(&self, offset: usize, data: &mut [u8]) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot read from remote mappings");

        self.check(offset, data.len())?;
        // SAFETY: the bounds have been checked above.
        unsafe {
            let src = (self.as_ptr() as *const u8).add(offset);
            trycopy::try_copy(src, data.as_mut_ptr(), data.len())
                .map_err(SparseMappingError::Memory)
        }
    }

    /// Tries to read a type `T` from `offset`.
    pub fn read_plain<T: FromBytes + Immutable + KnownLayout>(
        &self,
        offset: usize,
    ) -> Result<T, SparseMappingError> {
        if matches!(size_of::<T>(), 1 | 2 | 4 | 8) {
            self.read_volatile(offset)
        } else {
            let mut obj = MaybeUninit::<T>::uninit();
            // SAFETY: `obj` is a valid target for writes.
            unsafe {
                self.read_at(
                    offset,
                    std::slice::from_raw_parts_mut(obj.as_mut_ptr().cast::<u8>(), size_of::<T>()),
                )?;
            }
            // SAFETY: `obj` was fully initialized by `read_at`.
            Ok(unsafe { obj.assume_init() })
        }
    }

    /// Tries to fill a region of the sparse mapping with `val`.
    pub fn fill_at(&self, offset: usize, val: u8, len: usize) -> Result<(), SparseMappingError> {
        assert!(self.is_local(), "cannot fill remote mappings");

        self.check(offset, len)?;
        // SAFETY: the bounds have been checked above.
        unsafe {
            let dest = self.as_ptr().cast::<u8>().add(offset);
            trycopy::try_write_bytes(dest, val, len).map_err(SparseMappingError::Memory)
        }
    }

    /// Gets a slice for accessing the mapped data directly.
    ///
    /// This is safe from a Rust memory model perspective, since the underlying
    /// VA is either mapped and is owned in a shared state by this object (in
    /// which case &[AtomicU8] access from multiple threads is fine), or the VA
    /// is not mapped but is reserved and so will not be mapped by another Rust
    /// object.
    ///
    /// In the latter case, actually accessing the data may cause a fault, which
    /// will likely lead to a process crash, so care must nonetheless be taken
    /// when using this method.
    pub fn atomic_slice(&self, start: usize, len: usize) -> &[AtomicU8] {
        assert!(self.len() >= start && self.len() - start >= len);
        // SAFETY: slice is within the mapped range
        unsafe { std::slice::from_raw_parts((self.as_ptr() as *const AtomicU8).add(start), len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static BUF: [u8; 65536] = [0xcc; 65536];

    fn test_with(range_size: usize) {
        let page_size = SparseMapping::page_size();

        let mapping = SparseMapping::new(range_size).unwrap();
        mapping.alloc(page_size, page_size).unwrap();
        let slice = unsafe {
            std::slice::from_raw_parts_mut(mapping.as_ptr().add(page_size).cast::<u8>(), page_size)
        };
        slice.copy_from_slice(&BUF[..page_size]);
        mapping.unmap(page_size, page_size).unwrap();

        mapping.alloc(range_size - page_size, page_size).unwrap();
        let slice = unsafe {
            std::slice::from_raw_parts_mut(
                mapping.as_ptr().add(range_size - page_size).cast::<u8>(),
                page_size,
            )
        };
        slice.copy_from_slice(&BUF[..page_size]);
        mapping.unmap(range_size - page_size, page_size).unwrap();
        drop(mapping);
    }

    #[test]
    fn test_sparse_mapping() {
        test_with(0x100000);
        test_with(0x200000);
        test_with(0x200000 + SparseMapping::page_size());
        test_with(0x40000000);
        test_with(0x40000000 + SparseMapping::page_size());
    }

    #[test]
    fn test_overlapping_mappings() {
        #![expect(clippy::identity_op)]

        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(0x10 * page_size).unwrap();
        mapping.alloc(0x1 * page_size, 0x4 * page_size).unwrap();
        mapping.alloc(0x1 * page_size, 0x2 * page_size).unwrap();
        mapping.alloc(0x2 * page_size, 0x3 * page_size).unwrap();
        mapping.alloc(0, 0x10 * page_size).unwrap();
        mapping.alloc(0x8 * page_size, 0x8 * page_size).unwrap();
        mapping.unmap(0xc * page_size, 0x2 * page_size).unwrap();
        mapping.alloc(0x9 * page_size, 0x4 * page_size).unwrap();
        mapping.unmap(0x3 * page_size, 0xb * page_size).unwrap();

        mapping.alloc(0x5 * page_size, 0x4 * page_size).unwrap();
        mapping.alloc(0x6 * page_size, 0x2 * page_size).unwrap();
        mapping.alloc(0x6 * page_size, 0x1 * page_size).unwrap();
        mapping.alloc(0x4 * page_size, 0x3 * page_size).unwrap();

        let shmem = alloc_shared_memory(0x4 * page_size).unwrap();
        mapping
            .map_file(0x5 * page_size, 0x4 * page_size, &shmem, 0, true)
            .unwrap();
        mapping
            .map_file(0x6 * page_size, 0x2 * page_size, &shmem, 0, true)
            .unwrap();
        mapping
            .map_file(0x6 * page_size, 0x1 * page_size, &shmem, 0, true)
            .unwrap();
        mapping
            .map_file(0x4 * page_size, 0x3 * page_size, &shmem, 0, true)
            .unwrap();

        drop(mapping);
    }

    #[test]
    fn test_decommit_zeros_pages() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Allocate and write a pattern.
        mapping.alloc(0, 4 * page_size).unwrap();
        let pattern = vec![0xABu8; page_size];
        mapping.write_at(0, &pattern).unwrap();
        mapping.write_at(page_size, &pattern).unwrap();

        // Verify data is present.
        let mut buf = vec![0u8; page_size];
        mapping.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, pattern);

        // Decommit the first page.
        mapping.decommit(0, page_size).unwrap();

        // Read it back — should be zeros (on Linux, kernel gives zero pages;
        // on Windows, the page is decommitted so we skip this read there).
        #[cfg(unix)]
        {
            let mut buf = vec![0xFFu8; page_size];
            mapping.read_at(0, &mut buf).unwrap();
            assert!(
                buf.iter().all(|&b| b == 0),
                "decommitted page should be zeros"
            );
        }

        // Second page should still have its data.
        let mut buf2 = vec![0u8; page_size];
        mapping.read_at(page_size, &mut buf2).unwrap();
        assert_eq!(buf2, pattern);
    }

    #[test]
    fn test_commit_after_decommit() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Allocate and write data.
        mapping.alloc(0, 4 * page_size).unwrap();
        let pattern = vec![0xCDu8; page_size];
        mapping.write_at(0, &pattern).unwrap();

        // Decommit then recommit.
        mapping.decommit(0, page_size).unwrap();
        mapping.commit(0, page_size).unwrap();

        // After recommit, the page should be accessible and zeroed.
        let mut buf = vec![0xFFu8; page_size];
        mapping.read_at(0, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0),
            "recommitted page should be zeros"
        );
    }

    #[test]
    fn test_commit_idempotent() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Allocate (commit) pages.
        mapping.alloc(0, 4 * page_size).unwrap();

        // Commit the same range again — should be a no-op, no error.
        mapping.commit(0, 4 * page_size).unwrap();
        mapping.commit(0, page_size).unwrap();
        mapping.commit(page_size, page_size).unwrap();

        // Write and read to verify pages still work.
        let pattern = vec![0xEFu8; page_size];
        mapping.write_at(0, &pattern).unwrap();
        let mut buf = vec![0u8; page_size];
        mapping.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, pattern);
    }
}
