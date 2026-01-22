// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Declaring unsafe trait functions for manual memory management.
#![expect(unsafe_code)]

/// Trait for mapping process memory into a partition.
pub trait PartitionMemoryMap: Send + Sync {
    /// Unmaps any ranges in the given guest physical address range.
    ///
    /// The specified range may overlap zero, one, or many ranges mapped with
    /// `map_range`. Any overlapped ranges must be completely contained in the
    /// specified range.
    ///
    /// The hypervisor must ensure that this operation does not fail as long as
    /// the preconditions are satisfied.
    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()>;

    /// Maps a range from process memory into the VM.
    ///
    /// This may fail if the range overlaps any other mapped range.
    ///
    /// # Safety
    /// The caller must ensure that the VA region (data..data+size) is not
    /// reused for the lifetime of this mapping.
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()>;

    /// Prefetches any memory in the given range so that it can be accessed
    /// quickly by the partition without exits.
    fn prefetch_range(&self, _addr: u64, _size: u64) -> anyhow::Result<()> {
        Ok(())
    }

    /// Pins a range in memory so that it can be accessed by assigned devices.
    fn pin_range(&self, _addr: u64, _size: u64) -> anyhow::Result<()> {
        Ok(())
    }

    /// Maps a range residing in a remote process.
    ///
    /// This may fail if the range overlaps any other mapped range.
    ///
    /// # Safety
    /// The caller must ensure that the VA region (data..data+size) within
    /// `process` is not reused for the lifetime of this mapping.
    #[cfg(windows)]
    unsafe fn map_remote_range(
        &self,
        process: std::os::windows::io::BorrowedHandle<'_>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()>;
}
