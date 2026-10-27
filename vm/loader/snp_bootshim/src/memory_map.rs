// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The UEFI memory map and page allocator that the shim gives to the Linux
//! EFI stub.
//!
//! The map describes all RAM. Memory that the image did not import and that
//! nothing has used yet is `EfiUnacceptedMemoryType`. Allocations come from
//! accepted free memory first. When that does not fit, the allocator accepts
//! unaccepted memory on demand. Linux accepts the remaining unaccepted memory
//! lazily, after boot.

use arrayvec::ArrayVec;
use memory_range::MemoryRange;
use uefi_specs::uefi::boot::EFI_MEMORY_RUNTIME;
use uefi_specs::uefi::boot::EFI_MEMORY_WB;
use uefi_specs::uefi::boot::EFI_PAGE_SIZE;
use uefi_specs::uefi::boot::EfiMemoryDescriptor;
use uefi_specs::uefi::boot::EfiMemoryType;

/// The maximum number of entries. The map holds the host RAM ranges, the
/// omitted ranges from the parameter page, the image's typed regions, and the
/// allocations of the EFI stub.
const CAPACITY: usize = 256;

/// A memory-map failure, which maps directly to an EFI status.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Error {
    /// No memory fits the request, or the map is full.
    OutOfResources,
    /// The request is invalid.
    InvalidParameter,
    /// The pages were not allocated by the allocator.
    NotFound,
    /// Memory acceptance failed.
    Accept,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct Entry {
    range: MemoryRange,
    ty: EfiMemoryType,
    /// The allocator handed out this memory, so it can be freed.
    allocated: bool,
}

/// Where an allocation may lie.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum AllocateType {
    /// Anywhere.
    Any,
    /// At or below this address, inclusive.
    Max(u64),
    /// At exactly this address.
    Address(u64),
}

/// The memory map.
#[derive(Debug, Default)]
pub(crate) struct MemoryMap {
    entries: ArrayVec<Entry, CAPACITY>,
    key: u64,
}

impl MemoryMap {
    /// Returns a key that changes whenever the map changes.
    pub fn key(&self) -> u64 {
        self.key
    }

    /// Sets the type of `range`. RAM that the map does not describe yet is
    /// added.
    pub fn set(&mut self, range: MemoryRange, ty: EfiMemoryType) -> Result<(), Error> {
        self.set_entry(range, ty, false)
    }

    fn set_entry(
        &mut self,
        range: MemoryRange,
        ty: EfiMemoryType,
        allocated: bool,
    ) -> Result<(), Error> {
        if range.is_empty() {
            return Ok(());
        }
        if !range.start().is_multiple_of(EFI_PAGE_SIZE)
            || !range.end().is_multiple_of(EFI_PAGE_SIZE)
        {
            return Err(Error::InvalidParameter);
        }
        // Rebuild the entries in a new list, splitting the ones that overlap
        // `range`. The list has room for the two pieces of a split entry and
        // the new one.
        let mut entries = ArrayVec::<Entry, CAPACITY>::new();
        let mut inserted = false;
        let new = Entry {
            range,
            ty,
            allocated,
        };
        for entry in &self.entries {
            if !inserted && entry.range.start() >= range.start() {
                entries.try_push(new).map_err(|_| Error::OutOfResources)?;
                inserted = true;
            }
            let left = (entry.range.start() < range.start())
                .then(|| entry.range.start()..entry.range.end().min(range.start()));
            let right = (entry.range.end() > range.end())
                .then(|| entry.range.start().max(range.end())..entry.range.end());
            for piece in [left, right].into_iter().flatten() {
                let piece = MemoryRange::new(piece);
                {
                    if !inserted && piece.start() >= range.start() {
                        entries.try_push(new).map_err(|_| Error::OutOfResources)?;
                        inserted = true;
                    }
                    entries
                        .try_push(Entry {
                            range: piece,
                            ..*entry
                        })
                        .map_err(|_| Error::OutOfResources)?;
                }
            }
        }
        if !inserted {
            entries.try_push(new).map_err(|_| Error::OutOfResources)?;
        }

        // Merge neighbors of the same kind.
        self.entries.clear();
        for entry in entries {
            if let Some(last) = self.entries.last_mut() {
                if last.range.end() == entry.range.start()
                    && last.ty == entry.ty
                    && last.allocated == entry.allocated
                {
                    last.range = MemoryRange::new(last.range.start()..entry.range.end());
                    continue;
                }
            }
            self.entries.push(entry);
        }
        self.key += 1;
        Ok(())
    }

    /// Returns `true` if every page of `range` has one of the types in `types`.
    fn covered_by(&self, range: MemoryRange, types: &[EfiMemoryType]) -> bool {
        let mut cursor = range.start();
        for entry in &self.entries {
            if entry.range.end() <= cursor {
                continue;
            }
            if entry.range.start() > cursor || !types.contains(&entry.ty) {
                return false;
            }
            cursor = entry.range.end();
            if cursor >= range.end() {
                return true;
            }
        }
        range.is_empty()
    }

    /// Accepts the unaccepted parts of `range` and sets the type of all of
    /// `range`, which must lie in free or unaccepted memory.
    pub fn accept(
        &mut self,
        range: MemoryRange,
        ty: EfiMemoryType,
        accept: &mut dyn FnMut(MemoryRange) -> bool,
    ) -> Result<(), Error> {
        self.claim(range, ty, false, accept)
    }

    fn claim(
        &mut self,
        range: MemoryRange,
        ty: EfiMemoryType,
        allocated: bool,
        accept: &mut dyn FnMut(MemoryRange) -> bool,
    ) -> Result<(), Error> {
        // Updating the map adds at most two entries. Check for room before
        // accepting memory, so that accepted memory is never left marked as
        // unaccepted.
        if self.entries.len() + 2 > CAPACITY {
            return Err(Error::OutOfResources);
        }
        if !self.covered_by(
            range,
            &[
                EfiMemoryType::EFI_CONVENTIONAL_MEMORY,
                EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE,
            ],
        ) {
            return Err(Error::NotFound);
        }
        for entry in &self.entries {
            if entry.ty == EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE {
                let part = entry.range.intersection(&range);
                if !part.is_empty() && !accept(part) {
                    return Err(Error::Accept);
                }
            }
        }
        self.set_entry(range, ty, allocated)
    }

    /// Allocates `pages` pages of type `ty`.
    pub fn allocate(
        &mut self,
        allocate_type: AllocateType,
        ty: EfiMemoryType,
        pages: u64,
        accept: &mut dyn FnMut(MemoryRange) -> bool,
    ) -> Result<u64, Error> {
        if pages == 0 || !is_allocatable(ty) {
            return Err(Error::InvalidParameter);
        }
        let size = pages
            .checked_mul(EFI_PAGE_SIZE)
            .ok_or(Error::OutOfResources)?;
        let range = match allocate_type {
            AllocateType::Address(address) => address
                .checked_add(size)
                .filter(|_| address.is_multiple_of(EFI_PAGE_SIZE))
                .map(|end| MemoryRange::new(address..end))
                .ok_or(Error::NotFound)?,
            AllocateType::Any | AllocateType::Max(_) => {
                let limit = match allocate_type {
                    AllocateType::Max(max) => max.saturating_add(1),
                    _ => u64::MAX,
                };
                // Prefer accepted memory, as firmware allocates from the top.
                self.find_free(EfiMemoryType::EFI_CONVENTIONAL_MEMORY, size, limit)
                    .or_else(|| {
                        self.find_free(EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE, size, limit)
                    })
                    .ok_or(Error::OutOfResources)?
            }
        };
        self.claim(range, ty, true, accept)?;
        Ok(range.start())
    }

    /// Finds the highest range of `size` bytes below `limit` in one entry of
    /// type `ty`.
    fn find_free(&self, ty: EfiMemoryType, size: u64, limit: u64) -> Option<MemoryRange> {
        self.entries.iter().rev().find_map(|entry| {
            if entry.ty != ty {
                return None;
            }
            let end = entry.range.end().min(limit) & !(EFI_PAGE_SIZE - 1);
            let start = end.checked_sub(size)?;
            (start >= entry.range.start()).then(|| MemoryRange::new(start..end))
        })
    }

    /// Frees `pages` pages at `address`, which the allocator must have handed
    /// out.
    pub fn free(&mut self, address: u64, pages: u64) -> Result<(), Error> {
        let range = pages
            .checked_mul(EFI_PAGE_SIZE)
            .and_then(|size| address.checked_add(size))
            .filter(|_| pages != 0 && address.is_multiple_of(EFI_PAGE_SIZE))
            .map(|end| MemoryRange::new(address..end))
            .ok_or(Error::NotFound)?;
        let mut cursor = range.start();
        for entry in &self.entries {
            if entry.range.end() <= cursor {
                continue;
            }
            if entry.range.start() > cursor || !entry.allocated {
                return Err(Error::NotFound);
            }
            cursor = entry.range.end();
            if cursor >= range.end() {
                return self.set_entry(range, EfiMemoryType::EFI_CONVENTIONAL_MEMORY, false);
            }
        }
        Err(Error::NotFound)
    }

    /// Returns the descriptors that the map reports to the OS.
    pub fn descriptors(&self) -> impl ExactSizeIterator<Item = EfiMemoryDescriptor> + '_ {
        self.entries.iter().map(|entry| EfiMemoryDescriptor {
            typ: entry.ty,
            _pad: 0,
            physical_start: entry.range.start(),
            virtual_start: 0,
            number_of_pages: entry.range.len() / EFI_PAGE_SIZE,
            attribute: attribute(entry.ty),
        })
    }
}

/// Returns `true` if the EFI stub may allocate memory of type `ty`.
fn is_allocatable(ty: EfiMemoryType) -> bool {
    matches!(
        ty,
        EfiMemoryType::EFI_LOADER_CODE
            | EfiMemoryType::EFI_LOADER_DATA
            | EfiMemoryType::EFI_BOOT_SERVICES_CODE
            | EfiMemoryType::EFI_BOOT_SERVICES_DATA
            | EfiMemoryType::EFI_RUNTIME_SERVICES_CODE
            | EfiMemoryType::EFI_RUNTIME_SERVICES_DATA
            | EfiMemoryType::EFI_ACPI_RECLAIM_MEMORY
            | EfiMemoryType::EFI_ACPI_MEMORY_NVS
            | EfiMemoryType::EFI_RESERVED_MEMORY_TYPE
    )
}

fn attribute(ty: EfiMemoryType) -> u64 {
    match ty {
        EfiMemoryType::EFI_RUNTIME_SERVICES_CODE | EfiMemoryType::EFI_RUNTIME_SERVICES_DATA => {
            EFI_MEMORY_WB | EFI_MEMORY_RUNTIME
        }
        _ => EFI_MEMORY_WB,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_with_tracing::test;

    const MB: u64 = 1 << 20;
    const CONVENTIONAL: EfiMemoryType = EfiMemoryType::EFI_CONVENTIONAL_MEMORY;
    const UNACCEPTED: EfiMemoryType = EfiMemoryType::EFI_UNACCEPTED_MEMORY_TYPE;
    const LOADER_DATA: EfiMemoryType = EfiMemoryType::EFI_LOADER_DATA;
    const BS_DATA: EfiMemoryType = EfiMemoryType::EFI_BOOT_SERVICES_DATA;

    fn range(start: u64, end: u64) -> MemoryRange {
        MemoryRange::new(start..end)
    }

    fn layout(map: &MemoryMap) -> Vec<(u64, u64, EfiMemoryType)> {
        map.descriptors()
            .map(|d| {
                (
                    d.physical_start,
                    d.physical_start + d.number_of_pages * EFI_PAGE_SIZE,
                    d.typ,
                )
            })
            .collect()
    }

    fn no_accept() -> impl FnMut(MemoryRange) -> bool {
        |range| panic!("unexpected acceptance of {range}")
    }

    /// 0-16 MiB imported, 16-32 MiB free, 32-64 MiB unaccepted.
    fn test_map() -> MemoryMap {
        let mut map = MemoryMap::default();
        map.set(range(0, 64 * MB), BS_DATA).unwrap();
        map.set(range(16 * MB, 32 * MB), CONVENTIONAL).unwrap();
        map.set(range(32 * MB, 64 * MB), UNACCEPTED).unwrap();
        map
    }

    #[test]
    fn set_splits_and_merges() {
        let mut map = test_map();
        assert_eq!(
            layout(&map),
            [
                (0, 16 * MB, BS_DATA),
                (16 * MB, 32 * MB, CONVENTIONAL),
                (32 * MB, 64 * MB, UNACCEPTED),
            ]
        );
        map.set(range(20 * MB, 21 * MB), LOADER_DATA).unwrap();
        map.set(range(20 * MB, 21 * MB), CONVENTIONAL).unwrap();
        assert_eq!(layout(&map)[1], (16 * MB, 32 * MB, CONVENTIONAL));
        assert_eq!(layout(&map).len(), 3);
    }

    #[test]
    fn set_adds_ram_in_order() {
        let mut map = MemoryMap::default();
        map.set(range(8 * MB, 9 * MB), CONVENTIONAL).unwrap();
        map.set(range(0, MB), BS_DATA).unwrap();
        map.set(range(4 * MB, 5 * MB), UNACCEPTED).unwrap();
        assert_eq!(
            layout(&map),
            [
                (0, MB, BS_DATA),
                (4 * MB, 5 * MB, UNACCEPTED),
                (8 * MB, 9 * MB, CONVENTIONAL),
            ]
        );
    }

    #[test]
    fn key_changes_on_every_update() {
        let mut map = test_map();
        let key = map.key();
        let address = map
            .allocate(AllocateType::Any, LOADER_DATA, 1, &mut no_accept())
            .unwrap();
        assert_ne!(map.key(), key);
        let key = map.key();
        map.free(address, 1).unwrap();
        assert_ne!(map.key(), key);
    }

    #[test]
    fn allocates_accepted_memory_from_the_top() {
        let mut map = test_map();
        let address = map
            .allocate(AllocateType::Any, LOADER_DATA, 2, &mut no_accept())
            .unwrap();
        assert_eq!(address, 32 * MB - 2 * EFI_PAGE_SIZE);
        let address = map
            .allocate(
                AllocateType::Max(20 * MB - 1),
                LOADER_DATA,
                1,
                &mut no_accept(),
            )
            .unwrap();
        assert_eq!(address, 20 * MB - EFI_PAGE_SIZE);
    }

    #[test]
    fn accepts_on_demand_when_accepted_memory_is_full() {
        let mut map = test_map();
        let mut accepted = Vec::new();
        let address = map
            .allocate(AllocateType::Any, LOADER_DATA, 4096 + 1, &mut |range| {
                accepted.push(range);
                true
            })
            .unwrap();
        assert_eq!(address, 64 * MB - (4096 + 1) * EFI_PAGE_SIZE);
        assert_eq!(accepted, [range(address, 64 * MB)]);
    }

    #[test]
    fn allocate_address_accepts_only_unaccepted_parts() {
        let mut map = test_map();
        let mut accepted = Vec::new();
        let address = map
            .allocate(
                AllocateType::Address(30 * MB),
                EfiMemoryType::EFI_LOADER_CODE,
                (4 * MB) / EFI_PAGE_SIZE,
                &mut |range| {
                    accepted.push(range);
                    true
                },
            )
            .unwrap();
        assert_eq!(address, 30 * MB);
        assert_eq!(accepted, [range(32 * MB, 34 * MB)]);
    }

    #[test]
    fn allocate_address_rejects_used_memory() {
        let mut map = test_map();
        assert_eq!(
            map.allocate(
                AllocateType::Address(15 * MB),
                LOADER_DATA,
                (2 * MB) / EFI_PAGE_SIZE,
                &mut no_accept()
            ),
            Err(Error::NotFound)
        );
    }

    #[test]
    fn allocation_fails_cleanly_when_acceptance_fails() {
        let mut map = test_map();
        let before = layout(&map);
        assert_eq!(
            map.allocate(AllocateType::Address(40 * MB), LOADER_DATA, 1, &mut |_| {
                false
            }),
            Err(Error::Accept)
        );
        assert_eq!(layout(&map), before);
    }

    #[test]
    fn rejects_invalid_allocations() {
        let mut map = test_map();
        for (ty, pages) in [
            (LOADER_DATA, 0),
            (CONVENTIONAL, 1),
            (UNACCEPTED, 1),
            (EfiMemoryType(0x8000_0000), 1),
        ] {
            assert_eq!(
                map.allocate(AllocateType::Any, ty, pages, &mut no_accept()),
                Err(Error::InvalidParameter)
            );
        }
        assert_eq!(
            map.allocate(AllocateType::Any, LOADER_DATA, 1 << 20, &mut |_| true),
            Err(Error::OutOfResources)
        );
    }

    #[test]
    fn frees_only_allocated_pages() {
        let mut map = test_map();
        let address = map
            .allocate(AllocateType::Any, LOADER_DATA, 4, &mut no_accept())
            .unwrap();
        // Free the head and the tail, as aligned allocations do.
        map.free(address, 1).unwrap();
        map.free(address + 3 * EFI_PAGE_SIZE, 1).unwrap();
        assert_eq!(map.free(address, 1), Err(Error::NotFound));
        assert_eq!(map.free(0, 1), Err(Error::NotFound));
        map.free(address + EFI_PAGE_SIZE, 2).unwrap();
        assert_eq!(layout(&map), layout(&test_map()));
    }

    #[test]
    fn runtime_memory_has_the_runtime_attribute() {
        let mut map = MemoryMap::default();
        map.set(range(0, MB), EfiMemoryType::EFI_RUNTIME_SERVICES_CODE)
            .unwrap();
        map.set(range(MB, 2 * MB), CONVENTIONAL).unwrap();
        let attributes = map.descriptors().map(|d| d.attribute).collect::<Vec<_>>();
        assert_eq!(
            attributes,
            [EFI_MEMORY_WB | EFI_MEMORY_RUNTIME, EFI_MEMORY_WB]
        );
    }
}
