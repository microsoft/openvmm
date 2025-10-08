// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the partition mapper, which is responsible for mapping regions
//! into VM partitions.

// UNSAFETY: Calling unsafe partition memory mapping functions.
#![expect(unsafe_code)]

use crate::mapping_manager::VaMapper;
use crate::region_manager::MapParams;
use memory_range::MemoryRange;
use std::sync::Arc;
use std::sync::Weak;
use thiserror::Error;
use virt::PartitionMemoryMap;

/// The partition mapper.
#[derive(Debug)]
pub struct PartitionMapper {
    partition: Weak<dyn PartitionMemoryMap>,
    mapper: Arc<VaMapper>,
    offset: u64,
    pin_mappings: bool,
}

/// Failure to map a region.
#[derive(Debug, Error)]
pub enum PartitionMapperError {
    #[error("failed to map range to partition")]
    Map(#[source] anyhow::Error),
    #[error("failed to pin range to partition")]
    Pin(#[source] anyhow::Error),
}

impl PartitionMapper {
    /// Returns a new partition mapper.
    ///
    /// If `pin_mappings`, call [`PartitionMemoryMap::pin_range`] on any region that is mapped.
    pub fn new(
        partition: &Arc<dyn PartitionMemoryMap>,
        mapper: Arc<VaMapper>,
        offset: u64,
        pin_mappings: bool,
    ) -> Self {
        Self {
            partition: Arc::downgrade(partition),
            mapper,
            offset,
            pin_mappings,
        }
    }

    /// Maps a region.
    pub async fn map_region(
        &self,
        range: MemoryRange,
        params: MapParams,
    ) -> Result<(), PartitionMapperError> {
        // Ensure this range does not exceed the mapper's reserved VA range.
        assert!(range.end() <= self.mapper.len() as u64);

        // If the partition is gone then there is nothing to do.
        let Some(partition) = self.partition.upgrade() else {
            return Ok(());
        };

        // Wait for the range to be mapped so that any second level faults can
        // be satisfied by the kernel/hypervisor without VMM interaction.
        let _ = self.mapper.ensure_mapped(range).await;

        let addr = range.start().checked_add(self.offset).unwrap();
        let size = range.len() as usize;
        let data = self.mapper.as_ptr().wrapping_add(range.start() as usize);

        match self.mapper.process() {
            None => {
                // SAFETY: Mapper will ensure the VA range is reserved (but not
                // necessarily mapped) for its lifetime.
                unsafe { partition.map_range(data, size, addr, params.writable, params.executable) }
            }
            Some(process) => {
                match process {
                    #[cfg(not(windows))]
                    _ => unreachable!(),
                    #[cfg(windows)]
                    process => {
                        // SAFETY: Mapper will ensure the VA range is reserved (but not
                        // necessarily mapped) for its lifetime.
                        unsafe {
                            partition.map_remote_range(
                                process.as_handle(),
                                data,
                                size,
                                addr,
                                params.writable,
                                params.executable,
                            )
                        }
                    }
                }
            }
        }
        .map_err(PartitionMapperError::Map)?;

        if params.prefetch {
            if let Err(err) = partition.prefetch_range(addr, size as u64) {
                tracing::warn!(
                    error = err.as_ref() as &dyn std::error::Error,
                    addr,
                    size,
                    "prefetch failed"
                );
            }
        }

        if self.pin_mappings {
            if let Err(err) = partition.pin_range(addr, size as u64) {
                // Unmap the range to ensure we stay in a consistent state.
                partition
                    .unmap_range(addr, size as u64)
                    .expect("unmap cannot fail");
                return Err(PartitionMapperError::Pin(err));
            }
        }

        Ok(())
    }

    /// Unmaps regions in `range`.
    ///
    /// `range` may overlap zero, one, or many regions that were mapped with
    /// `map_region`, but it must fully contain any regions it overlaps.
    ///
    /// This cannot fail, but on some hypervisors, it may panic on partial
    /// region unmap.
    pub fn unmap_region(&mut self, range: MemoryRange) {
        if let Some(partition) = self.partition.upgrade() {
            partition
                .unmap_range(range.start().checked_add(self.offset).unwrap(), range.len())
                .expect("unmap cannot fail");
        }
    }

    /// Notifies the partition that a new mapping has been mapped into a
    /// previously mapped region.
    pub async fn notify_new_mapping(&mut self, range: MemoryRange) {
        // Ensure the VA range has been mapped for this mapping so that the
        // kernel can update the hypervisor's SLAT on page fault.
        let _ = self.mapper.ensure_mapped(range).await;
    }
}

impl Drop for PartitionMapper {
    fn drop(&mut self) {
        // Ensure everything is unmapped from the partition since the underlying
        // VA is going away.
        self.unmap_region(MemoryRange::new(0..self.mapper.len() as u64));
    }
}
