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
            .filter(|end| *end <= self.len)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "device memory region {:?}: offset {offset:#x}, len {len:#x} exceeds region len {:#x}",
                        self.debug_name, self.len
                    ),
                )
            })?;
        (offset..end)
            .try_into()
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    anyhow::Error::new(err).context(format!(
                        "device memory region {:?}: invalid offset {offset:#x}, len {len:#x}, region len {:#x}",
                        self.debug_name, self.len
                    )),
                )
            })
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
        let mappable = cfg_select! {
            unix => section.as_fd().try_clone_to_owned()?,
            windows => section.as_handle().try_clone_to_owned()?,
        };

        let range = self.validated_memory_range(offset, len)?;
        let new_mapping = DeviceMapping {
            range,
            file_offset,
            mappable: mappable.into(),
            writable,
        };

        let mut state = self.state.lock();
        for mapping in &state.mappings {
            if mapping.range.overlaps(&new_mapping.range) {
                todo!("support overlapping mappings");
            }
        }

        if let Some(handle) = &state.handle {
            if let Err(e) = block_on(handle.add_mapping(
                new_mapping.range,
                MappingBacking::File {
                    mappable: new_mapping.mappable.clone(),
                    file_offset: new_mapping.file_offset,
                },
                new_mapping.writable,
                MemoryPolicy::none(),
            )) {
                return Err(io::Error::other(e));
            }
        }
        state.mappings.push(new_mapping);
        Ok(())
    }

    fn unmap(&self, offset: usize, len: usize) -> io::Result<()> {
        let range = self.validated_memory_range(offset, len)?;
        let mut state = self.state.lock();
        if let Some(mapping) = state
            .mappings
            .iter()
            .find(|mapping| !range.contains(&mapping.range) && range.overlaps(&mapping.range))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "device memory region {:?}: offset {offset:#x}, len {len:#x}, region len {:#x}, unmap {range:?} only partially covers mapping {:?}",
                    self.debug_name, self.len, mapping.range
                ),
            ));
        }
        state
            .mappings
            .retain(|mapping| !range.contains(&mapping.range));

        if let Some(handle) = &state.handle {
            block_on(handle.remove_mappings(range));
        }
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
    use super::*;
    use crate::mapping_manager::MappingManager;
    use crate::region_manager::RegionManager;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use test_with_tracing::test;

    #[async_test]
    async fn rejected_device_mapping_changes_preserve_existing_ranges(spawn: impl Spawn) {
        let mapping_manager = MappingManager::new_without_primary(&spawn, 0x10000, None);
        let region_manager = RegionManager::new(&spawn, mapping_manager.client().clone());
        let region = DeviceMemoryRegion {
            len: 0x4000,
            debug_name: "test-device".into(),
            region_manager: region_manager.client().clone(),
            state: Mutex::new(DeviceRegionState {
                handle: None,
                mappings: Vec::new(),
            }),
        };
        let backing = sparse_mmap::alloc_shared_memory(0x4000, "test-device").unwrap();
        region.map(0, &backing, 0, 0x2000, true).unwrap();
        region.map(0x3000, &backing, 0x3000, 0x1000, true).unwrap();
        for (error, offset) in [
            (
                region.map(0x4000, &backing, 0, 0x1000, true).unwrap_err(),
                0x4000,
            ),
            (region.unmap(usize::MAX, 0x1000).unwrap_err(), usize::MAX),
            (region.unmap(0x1000, 0x1000).unwrap_err(), 0x1000),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            let message = error.to_string();
            for detail in [
                "test-device".to_owned(),
                format!("offset {offset:#x}"),
                "len 0x1000".to_owned(),
                "region len 0x4000".to_owned(),
            ] {
                assert!(message.contains(&detail), "{message}");
            }
            if offset == 0x1000 {
                assert!(
                    message.contains(&format!("{:?}", MemoryRange::new(0..0x2000))),
                    "{message}"
                );
            }
        }
        assert_eq!(
            region
                .state
                .lock()
                .mappings
                .iter()
                .map(|mapping| mapping.range)
                .collect::<Vec<_>>(),
            [
                MemoryRange::new(0..0x2000),
                MemoryRange::new(0x3000..0x4000),
            ]
        );
        region.unmap(0, 0x2000).unwrap();
        region.map(0, &backing, 0, 0x2000, true).unwrap();
        assert_eq!(
            region
                .state
                .lock()
                .mappings
                .iter()
                .map(|mapping| mapping.range)
                .collect::<Vec<_>>(),
            [
                MemoryRange::new(0x3000..0x4000),
                MemoryRange::new(0..0x2000),
            ]
        );
    }
}
