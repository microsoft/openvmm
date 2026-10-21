// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This implements the [`MemoryMapper`] trait and related functionality for
//! [`GuestMemoryManager`](super::GuestMemoryManager).

use super::DEVICE_PRIORITY;
use crate::mapping_manager::Mappable;
use crate::mapping_manager::MappingBacking;
use crate::mapping_manager::MemoryPolicy;
use crate::region_manager::MapParams;
use crate::region_manager::RegionHandle;
use crate::region_manager::RegionManagerClient;
use futures::executor::block_on;
use guestmem::MappableGuestMemory;
use guestmem::MappedMemoryRegion;
use guestmem::MemoryMapper;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::io;
use std::sync::Arc;

/// A [`MemoryMapper`] implementation for
/// [`GuestMemoryManager`](super::GuestMemoryManager).
#[derive(Clone, Debug)]
pub struct DeviceMemoryMapper {
    region_manager: RegionManagerClient,
}

impl DeviceMemoryMapper {
    pub(super) fn new(region_manager: RegionManagerClient) -> Self {
        Self { region_manager }
    }
}

impl MemoryMapper for DeviceMemoryMapper {
    fn new_region(
        &self,
        len: usize,
        debug_name: String,
    ) -> io::Result<(Box<dyn MappableGuestMemory>, Arc<dyn MappedMemoryRegion>)> {
        let region = Arc::new(DeviceMemoryRegion {
            len,
            debug_name,
            region_manager: self.region_manager.clone(),
            state: Mutex::new(DeviceRegionState {
                handle: None,
                mappings: Vec::new(),
            }),
        });

        Ok((Box::new(DeviceMemoryControl(region.clone())), region))
    }
}

#[derive(Debug)]
struct DeviceMemoryRegion {
    debug_name: String,
    len: usize,
    region_manager: RegionManagerClient,
    state: Mutex<DeviceRegionState>,
}

#[derive(Debug)]
struct DeviceRegionState {
    handle: Option<RegionHandle>,
    mappings: Vec<DeviceMapping>,
}

#[derive(Debug)]
struct DeviceMapping {
    range: MemoryRange,
    file_offset: u64,
    mappable: Mappable,
    writable: bool,
}

impl DeviceMemoryRegion {
    fn validated_memory_range(&self, offset: usize, len: usize) -> io::Result<MemoryRange> {
        let end = offset
            .checked_add(len)
            .filter(|&end| len != 0 && end <= self.len)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid device memory range")
            })?;
        (offset..end)
            .try_into()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
    }
}

impl DeviceRegionState {
    fn add_mapping(&mut self, mapping: DeviceMapping) -> io::Result<()> {
        if let Some(handle) = &self.handle {
            block_on(handle.add_mapping(
                mapping.range,
                MappingBacking::File {
                    mappable: mapping.mappable.clone(),
                    file_offset: mapping.file_offset,
                },
                mapping.writable,
                MemoryPolicy::none(),
            ))
            .map_err(io::Error::other)?;
        }
        self.mappings.push(mapping);
        Ok(())
    }

    /// Removes `range` from the region. Mappings that partially overlap
    /// `range` are split, leaving the unaffected portions mapped. virtiofs
    /// DAX relies on this to replace or upgrade existing mappings.
    /// Returns the removed portions so a failed replacement can restore them.
    fn remove_range(&mut self, range: MemoryRange) -> io::Result<Vec<DeviceMapping>> {
        if let Some(handle) = &self.handle {
            block_on(handle.remove_mappings(range)).map_err(io::Error::other)?;
        }

        let mut kept = Vec::new();
        let mut removed_mappings = Vec::new();
        for mapping in std::mem::take(&mut self.mappings) {
            if !mapping.range.overlaps(&range) {
                kept.push(mapping);
                continue;
            }

            let removed = mapping.range.intersection(&range);
            removed_mappings.push(DeviceMapping {
                range: removed,
                file_offset: mapping.file_offset + (removed.start() - mapping.range.start()),
                mappable: mapping.mappable.clone(),
                writable: mapping.writable,
            });
            if mapping.range.start() < removed.start() {
                kept.push(DeviceMapping {
                    range: MemoryRange::new(mapping.range.start()..removed.start()),
                    file_offset: mapping.file_offset,
                    mappable: mapping.mappable.clone(),
                    writable: mapping.writable,
                });
            }
            if mapping.range.end() > removed.end() {
                kept.push(DeviceMapping {
                    range: MemoryRange::new(removed.end()..mapping.range.end()),
                    file_offset: mapping.file_offset + (removed.end() - mapping.range.start()),
                    mappable: mapping.mappable,
                    writable: mapping.writable,
                });
            }
        }
        self.mappings = kept;
        Ok(removed_mappings)
    }
}

impl MappedMemoryRegion for DeviceMemoryRegion {
    fn map(
        &self,
        offset: usize,
        section: &dyn sparse_mmap::AsMappableRef,
        file_offset: u64,
        len: usize,
        writable: bool,
    ) -> io::Result<()> {
        let range = self.validated_memory_range(offset, len)?;
        if !file_offset.is_multiple_of(hvdef::HV_PAGE_SIZE)
            || file_offset.checked_add(range.len() - 1).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unaligned or overflowing backing range",
            ));
        }

        #[cfg(unix)]
        let mappable = section.as_fd().try_clone_to_owned()?;
        #[cfg(windows)]
        let mappable = section.as_handle().try_clone_to_owned()?;

        let new_mapping = DeviceMapping {
            range,
            file_offset,
            mappable: mappable.into(),
            writable,
        };

        let mut state = self.state.lock();
        let removed = state.remove_range(range)?;
        if let Err(mapping_error) = state.add_mapping(new_mapping) {
            for mapping in removed {
                let range = mapping.range;
                if let Err(error) = state.add_mapping(mapping) {
                    tracelimit::error_ratelimited!(
                        %range,
                        error = &error as &dyn std::error::Error,
                        "failed to restore device memory mapping during rollback"
                    );
                }
            }
            return Err(mapping_error);
        }
        Ok(())
    }

    fn unmap(&self, offset: usize, len: usize) -> io::Result<()> {
        let range = self.validated_memory_range(offset, len)?;
        self.state.lock().remove_range(range)?;
        Ok(())
    }
}

#[derive(Debug)]
struct DeviceMemoryControl(Arc<DeviceMemoryRegion>);

impl MappableGuestMemory for DeviceMemoryControl {
    fn map_to_guest(&mut self, gpa: u64, writable: bool) -> io::Result<()> {
        #[expect(clippy::await_holding_lock)] // Treat all this as sync for now.
        block_on(async {
            let mut state = self.0.state.lock();
            if let Some(handle) = state.handle.take() {
                handle.teardown().await;
            }
            let handle = self
                .0
                .region_manager
                .new_region(
                    self.0.debug_name.clone(),
                    MemoryRange::try_from(gpa..gpa.wrapping_add(self.0.len as u64))
                        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?,
                    DEVICE_PRIORITY,
                    crate::region_manager::MappingType::Device,
                )
                .await
                .map_err(io::Error::other)?;

            for mapping in &state.mappings {
                handle
                    .add_mapping(
                        mapping.range,
                        MappingBacking::File {
                            mappable: mapping.mappable.clone(),
                            file_offset: mapping.file_offset,
                        },
                        mapping.writable,
                        MemoryPolicy::none(),
                    )
                    .await
                    .map_err(io::Error::other)?;
            }

            handle
                .map(MapParams {
                    writable,
                    executable: true,
                    prefetch: false,
                })
                .await
                .map_err(io::Error::other)?;

            state.handle = Some(handle);
            Ok(())
        })
    }

    fn unmap_from_guest(&mut self) {
        let mut state = self.0.state.lock();
        if let Some(handle) = state.handle.take() {
            block_on(handle.teardown());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DeviceMapping;
    use super::DeviceMemoryMapper;
    use super::DeviceMemoryRegion;
    use super::DeviceRegionState;
    use crate::mapping_manager::MappingManager;
    use crate::region_manager::RegionManager;
    use futures::executor::block_on;
    use guestmem::GuestMemory;
    use guestmem::MappedMemoryRegion;
    use guestmem::MemoryMapper;
    use memory_range::MemoryRange;
    use pal_async::DefaultPool;
    use parking_lot::Mutex;
    use std::io;
    use test_with_tracing::test;

    fn with_region(test: impl FnOnce(&DeviceMemoryRegion)) {
        DefaultPool::run_with(|spawn| async move {
            let mapping_manager = MappingManager::new_without_primary(&spawn, 0x20000, None);
            let region_manager = RegionManager::new(&spawn, mapping_manager.client().clone());
            let region = DeviceMemoryRegion {
                debug_name: "test".into(),
                len: 0x10000,
                region_manager: region_manager.client().clone(),
                state: Mutex::new(DeviceRegionState {
                    handle: None,
                    mappings: Vec::new(),
                }),
            };
            test(&region);
        });
    }

    #[test]
    fn invalid_device_ranges_preserve_existing_mapping() {
        with_region(|region| {
            let section = sparse_mmap::alloc_shared_memory(0x10000, "test").unwrap();
            region.map(0, &section, 0, 0x10000, true).unwrap();

            for (offset, len) in [
                (0x4000, 0),
                (0x4001, 0x1000),
                (0x4000, 0x1001),
                (0xf000, 0x2000),
                (0x10000, 0x1000),
                (usize::MAX - 0xfff, 0x1000),
            ] {
                assert_eq!(
                    region
                        .map(offset, &section, 0, len, false)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::InvalidInput,
                    "map offset={offset:#x}, len={len:#x}",
                );
                assert_eq!(
                    region.unmap(offset, len).unwrap_err().kind(),
                    io::ErrorKind::InvalidInput,
                    "unmap offset={offset:#x}, len={len:#x}",
                );

                let state = region.state.lock();
                assert_eq!(state.mappings.len(), 1);
                assert_eq!(state.mappings[0].range, MemoryRange::new(0..0x10000));
                assert_eq!(state.mappings[0].file_offset, 0);
                assert!(state.mappings[0].writable);
            }
        });
    }

    #[test]
    fn invalid_backing_ranges_preserve_existing_mapping() {
        with_region(|region| {
            let section = sparse_mmap::alloc_shared_memory(0x10000, "test").unwrap();
            region.map(0, &section, 0, 0x10000, true).unwrap();

            for file_offset in [1, u64::MAX - 0xfff] {
                assert_eq!(
                    region
                        .map(0x4000, &section, file_offset, 0x2000, false)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::InvalidInput,
                );

                let state = region.state.lock();
                assert_eq!(state.mappings.len(), 1);
                assert_eq!(state.mappings[0].range, MemoryRange::new(0..0x10000));
                assert_eq!(state.mappings[0].file_offset, 0);
                assert!(state.mappings[0].writable);
            }
        });
    }

    #[test]
    fn mapping_at_device_boundary_can_be_replaced_and_unmapped() {
        with_region(|region| {
            let section = sparse_mmap::alloc_shared_memory(0x10000, "test").unwrap();
            region.map(0xf000, &section, 0xf000, 0x1000, false).unwrap();
            region.map(0xf000, &section, 0xf000, 0x1000, true).unwrap();

            {
                let state = region.state.lock();
                assert_eq!(state.mappings.len(), 1);
                assert_eq!(state.mappings[0].range, MemoryRange::new(0xf000..0x10000));
                assert_eq!(state.mappings[0].file_offset, 0xf000);
                assert!(state.mappings[0].writable);
            }

            region.unmap(0xf000, 0x1000).unwrap();
            assert!(region.state.lock().mappings.is_empty());
        });
    }

    #[test]
    fn failed_replacement_restores_active_mapping() {
        let (thread, spawn) = DefaultPool::spawn_on_thread("device-memory-test");
        {
            let mapping_manager = MappingManager::new_without_primary(&spawn, 0x20000, None);
            let region_manager = RegionManager::new(&spawn, mapping_manager.client().clone());
            let memory = GuestMemory::new(
                "test",
                block_on(mapping_manager.client().new_mapper(true)).unwrap(),
            );
            let (mut control, region) = DeviceMemoryMapper::new(region_manager.client().clone())
                .new_region(0x10000, "test".into())
                .unwrap();
            let section = sparse_mmap::alloc_shared_memory(0x20000, "test").unwrap();
            region.map(0, &section, 0x4000, 0x10000, true).unwrap();
            control.map_to_guest(0, true).unwrap();

            let pattern: Vec<_> = (0..0x10000).map(|i| (i / 0x1000) as u8).collect();
            memory.write_at(0, &pattern).unwrap();

            let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
            let read_only = sparse_mmap::new_mappable_from_file(&file, false, false).unwrap();
            for (offset, len) in [(0x4000, 0x4000), (0, 0x10000)] {
                assert!(region.map(offset, &read_only, 0, len, true).is_err());

                let mut data = vec![0; pattern.len()];
                memory.read_at(0, &mut data).unwrap();
                assert_eq!(data, pattern);
                memory
                    .write_at(offset as u64, &pattern[offset..offset + len])
                    .unwrap();
            }

            region.unmap(0, 0x10000).unwrap();
            assert!(memory.read_at(0, &mut [0]).is_err());
            region.map(0, &section, 0x4000, 0x10000, true).unwrap();
            let mut data = vec![0; pattern.len()];
            memory.read_at(0, &mut data).unwrap();
            assert_eq!(data, pattern);
            control.unmap_from_guest();
        }
        drop(spawn);
        thread.join().unwrap();
    }

    #[test]
    fn remove_range_preserves_unaffected_mapping_fragments() {
        let mappable = sparse_mmap::alloc_shared_memory(0x10000, "test")
            .unwrap()
            .into();
        let mut state = DeviceRegionState {
            handle: None,
            mappings: vec![DeviceMapping {
                range: MemoryRange::new(0..0x10000),
                file_offset: 0,
                mappable,
                writable: true,
            }],
        };

        state
            .remove_range(MemoryRange::new(0x4000..0x8000))
            .unwrap();

        assert_eq!(state.mappings.len(), 2);
        assert_eq!(state.mappings[0].range, MemoryRange::new(0..0x4000));
        assert_eq!(state.mappings[0].file_offset, 0);
        assert_eq!(state.mappings[1].range, MemoryRange::new(0x8000..0x10000));
        assert_eq!(state.mappings[1].file_offset, 0x8000);
    }
}
