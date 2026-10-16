// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the partition mapper, which maps regions into VM partitions.

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
    /// If `pin_mappings`, call [`PartitionMemoryMap::pin_range`] on each mapped region.
    pub fn new(
        partition: &Arc<dyn PartitionMemoryMap>,
        mapper: Arc<VaMapper>,
        offset: u64,
        pin_mappings: bool,
    ) -> Self {
        assert!(
            mapper.is_eager(),
            "partition mapper requires an eager VaMapper"
        );
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
        assert!(range.end() <= self.mapper.len() as u64);
        let Some(partition) = self.partition.upgrade() else {
            return Ok(());
        };
        let addr = range.start().checked_add(self.offset).unwrap();
        let size = range.len() as usize;
        let data = self.mapper.as_ptr().wrapping_add(range.start() as usize);
        match self.mapper.process() {
            None => {
                // SAFETY: The mapper retains the VA reservation for this mapping.
                unsafe { partition.map_range(data, size, addr, params.writable, params.executable) }
            }
            Some(process) => match process {
                #[cfg(not(windows))]
                _ => unreachable!(),
                #[cfg(windows)]
                process => {
                    // SAFETY: The mapper retains the remote VA reservation.
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
            },
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
                partition
                    .unmap_range(addr, size as u64)
                    .expect("unmap cannot fail");
                return Err(PartitionMapperError::Pin(err));
            }
        }
        Ok(())
    }

    /// Unmaps complete regions contained in `range`.
    ///
    /// This cannot fail, but some hypervisors panic on a partial region unmap.
    pub fn unmap_region(&mut self, range: MemoryRange) {
        if let Some(partition) = self.partition.upgrade() {
            partition
                .unmap_range(range.start().checked_add(self.offset).unwrap(), range.len())
                .expect("unmap cannot fail");
        }
    }
}

impl Drop for PartitionMapper {
    fn drop(&mut self) {
        // Unmap before releasing the VA reservation.
        self.unmap_region(MemoryRange::new(0..self.mapper.len() as u64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mapping_manager::MappingManager;
    use pal_async::task::Spawn;
    use parking_lot::Mutex;

    struct TestPartition {
        mapper: Weak<VaMapper>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Drop for TestPartition {
        fn drop(&mut self) {
            self.events.lock().push("partition closed");
        }
    }

    impl PartitionMemoryMap for TestPartition {
        unsafe fn map_range(
            &self,
            _data: *mut u8,
            _size: usize,
            _addr: u64,
            _writable: bool,
            _exec: bool,
        ) -> anyhow::Result<()> {
            self.events.lock().push("mapped");
            Ok(())
        }

        fn unmap_range(&self, _addr: u64, _size: u64) -> anyhow::Result<()> {
            assert!(self.mapper.upgrade().is_some());
            self.events.lock().push("unmapped");
            Ok(())
        }

        #[cfg(windows)]
        unsafe fn map_remote_range(
            &self,
            _process: std::os::windows::io::BorrowedHandle<'_>,
            _data: *mut u8,
            _size: usize,
            _addr: u64,
            _writable: bool,
            _exec: bool,
        ) -> anyhow::Result<()> {
            unreachable!()
        }
    }

    #[pal_async::async_test]
    async fn mapper_unmaps_while_backing_and_partition_are_alive(spawn: impl Spawn) {
        let manager = MappingManager::new_without_primary(&spawn, 0x10000, None);
        let mapper = manager.client().new_mapper(true).await.unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let partition: Arc<dyn PartitionMemoryMap> = Arc::new(TestPartition {
            mapper: Arc::downgrade(&mapper),
            events: events.clone(),
        });
        let weak = Arc::downgrade(&partition);
        drop(PartitionMapper::new(&partition, mapper, 0, false));
        assert_eq!(*events.lock(), ["unmapped"]);
        drop(partition);
        assert!(weak.upgrade().is_none());
        assert_eq!(*events.lock(), ["unmapped", "partition closed"]);
        drop(manager);
    }
}
