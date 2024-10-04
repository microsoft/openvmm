// Copyright (C) Microsoft Corporation. All rights reserved.

//! This implements the [`MemoryMapper`] trait and related functionality for
//! [`GuestMemoryManager`](super::GuestMemoryManager).

use super::DEVICE_PRIORITY;
use crate::mapping_manager::Mappable;
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
        (offset..offset.wrapping_add(len))
            .try_into()
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))
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
        #[cfg(unix)]
        let mappable = section.as_fd().try_clone_to_owned()?;
        #[cfg(windows)]
        let mappable = section.as_handle().try_clone_to_owned()?;

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
            block_on(handle.add_mapping(
                new_mapping.range,
                new_mapping.mappable.clone(),
                new_mapping.file_offset,
                new_mapping.writable,
            ));
        }
        state.mappings.push(new_mapping);
        Ok(())
    }

    fn unmap(&self, offset: usize, len: usize) -> io::Result<()> {
        let range = self.validated_memory_range(offset, len)?;
        let mut state = self.state.lock();
        state.mappings.retain(|mapping| {
            if !range.contains(&mapping.range) && range.overlaps(&mapping.range) {
                todo!("support overlapping mappings");
            }
            range.contains(&mapping.range)
        });

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
        let mut state = self.0.state.lock();
        #[allow(clippy::await_holding_lock)] // Treat all this as sync for now.
        block_on(async {
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
                )
                .await
                .map_err(|err| io::Error::new(io::ErrorKind::Other, err))?;

            for mapping in &state.mappings {
                handle
                    .add_mapping(
                        mapping.range,
                        mapping.mappable.clone(),
                        mapping.file_offset,
                        mapping.writable,
                    )
                    .await;
            }

            handle
                .map(MapParams {
                    writable,
                    executable: true,
                    prefetch: false,
                })
                .await;

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
