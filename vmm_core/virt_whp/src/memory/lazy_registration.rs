// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! On-demand GPA registration for large guest-memory ranges.
//!
//! With lazy registration, [`WhpMemoryMapper`] registers only a bounded
//! prefix of each mapping with WHP when it is mapped, and registers the rest
//! in fixed-size chunks when the guest first accesses them.

use super::EmulatedOverlayState;
use super::Mapping;
use super::SimpleMemoryMap;
use super::WhpMemoryMapper;
use crate::VtlPartition;
use memory_range::MemoryRange;
use parking_lot::Mutex;

impl WhpMemoryMapper {
    /// Creates a mapper that optionally registers large ranges with WHP as the
    /// guest accesses them.
    pub(crate) fn with_lazy_registration(with_overlays: bool, lazy_registration: bool) -> Self {
        if !lazy_registration {
            return Self::new(with_overlays);
        }
        Self {
            overlays: Some(Mutex::new(EmulatedOverlayState::new(lazy_registration))),
            overlays_supported: with_overlays,
        }
    }
}

impl VtlPartition {
    pub(crate) fn map_deferred_on_fault(&self, gpa: u64) -> anyhow::Result<bool> {
        self.mapper.map_deferred_on_fault(&self.whp, gpa)
    }
}

impl EmulatedOverlayState {
    // Bound synchronous restore registration to the smallest canonical guest
    // size, then amortize later registration in large-page-sized chunks.
    const INITIAL_REGISTRATION_BYTES: u64 = 64 * 1024 * 1024;
    const REGISTRATION_CHUNK_BYTES: u64 = 2 * 1024 * 1024;

    fn new(lazy_registration: bool) -> Self {
        Self {
            lazy_registration,
            mappings: Vec::new(),
            overlays: Vec::new(),
            registered_ranges: Vec::new(),
        }
    }

    /// Registers the initial part of a new `mapping` with WHP: all of it, or a
    /// bounded prefix with lazy registration.
    pub(super) fn map_initial_range(
        &mut self,
        p: &dyn SimpleMemoryMap,
        mapping: &Mapping,
    ) -> anyhow::Result<()> {
        let registered_end = if self.lazy_registration {
            mapping
                .range
                .start()
                .saturating_add(Self::INITIAL_REGISTRATION_BYTES)
                .min(mapping.range.end())
        } else {
            mapping.range.end()
        };
        let registered = MemoryRange::new(mapping.range.start()..registered_end);
        self.map_underlay_range(p, mapping, registered)?;
        if self.lazy_registration {
            self.record_registered_range(registered);
        }
        Ok(())
    }

    /// Unregisters the registered ranges within the unmapped `range`.
    pub(super) fn unmap_registered_ranges(&mut self, p: &dyn SimpleMemoryMap, range: MemoryRange) {
        let registered_ranges = std::mem::take(&mut self.registered_ranges);
        for registered in registered_ranges {
            if range.contains(&registered) {
                self.unmap_underlay_range(p, registered);
            } else {
                assert!(!range.overlaps(&registered));
                self.registered_ranges.push(registered);
            }
        }
    }

    /// Returns whether the mapping page under `gpa` is registered with WHP.
    pub(super) fn underlay_registered(&self, gpa: u64) -> bool {
        !self.lazy_registration
            || self
                .registered_ranges
                .iter()
                .any(|range| range.contains_addr(gpa))
    }

    pub(super) fn map_deferred_on_fault(
        &mut self,
        p: &dyn SimpleMemoryMap,
        gpa: u64,
    ) -> anyhow::Result<bool> {
        if !self.lazy_registration {
            return Ok(false);
        }
        let Some(mapping) = self
            .mappings
            .iter()
            .find(|mapping| mapping.range.contains_addr(gpa))
        else {
            return Ok(false);
        };
        if self
            .registered_ranges
            .iter()
            .any(|range| range.contains_addr(gpa))
        {
            return Ok(true);
        }

        let chunk_start = mapping.range.start()
            + (gpa - mapping.range.start()) / Self::REGISTRATION_CHUNK_BYTES
                * Self::REGISTRATION_CHUNK_BYTES;
        let chunk_end = chunk_start
            .saturating_add(Self::REGISTRATION_CHUNK_BYTES)
            .min(mapping.range.end());
        let range = MemoryRange::new(chunk_start..chunk_end);
        self.map_underlay_range(p, mapping, range)?;
        self.record_registered_range(range);
        Ok(true)
    }

    fn record_registered_range(&mut self, range: MemoryRange) {
        assert!(!range.is_empty());
        let index = self
            .registered_ranges
            .partition_point(|registered| registered.start() < range.start());
        if let Some(previous) = index
            .checked_sub(1)
            .and_then(|previous| self.registered_ranges.get(previous))
        {
            assert!(previous.end() <= range.start());
        }
        if let Some(next) = self.registered_ranges.get(index) {
            assert!(range.end() <= next.start());
        }
        self.registered_ranges.insert(index, range);
    }
}

#[cfg(guest_arch = "x86_64")]
impl crate::WhpProcessor<'_> {
    /// Registers the deferred RAM range under an unmapped-GPA access. Returns
    /// whether the range was registered, so the access can be retried.
    pub(crate) fn map_deferred_ram(
        &self,
        access: &whp::abi::WHV_MEMORY_ACCESS_CONTEXT,
        backing_type: &super::x86::GpaBackingType,
    ) -> bool {
        if access.AccessInfo.GpaUnmapped()
            && matches!(backing_type, super::x86::GpaBackingType::Ram { .. })
        {
            match self.current_vtlp().map_deferred_on_fault(access.Gpa) {
                Ok(true) => return true,
                Ok(false) => {}
                Err(err) => {
                    tracelimit::warn_ratelimited!(
                        gpa = access.Gpa,
                        error = ?err,
                        "failed to register deferred gpa range"
                    );
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::*;
    use sparse_mmap::alloc::Allocation;
    use test_with_tracing::test;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct MapCall {
        range: MemoryRange,
        data: usize,
    }

    #[derive(Debug, Default)]
    struct TestPartition {
        maps: Mutex<Vec<MapCall>>,
        unmaps: Mutex<Vec<MemoryRange>>,
    }

    impl SimpleMemoryMap for TestPartition {
        unsafe fn map_range(
            &self,
            _process: Option<BorrowedHandle<'_>>,
            data: *mut u8,
            size: usize,
            addr: u64,
            _writable: bool,
            _exec: bool,
        ) -> anyhow::Result<()> {
            self.maps.lock().push(MapCall {
                range: MemoryRange::new(addr..addr + size as u64),
                data: data.addr(),
            });
            Ok(())
        }

        fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
            self.unmaps.lock().push(MemoryRange::new(addr..addr + size));
            Ok(())
        }
    }

    #[test]
    fn lazy_registration_maps_a_bounded_prefix_and_fault_chunks() {
        const MIB: u64 = 1024 * 1024;
        let partition = TestPartition::default();
        let mapper = WhpMemoryMapper::with_lazy_registration(false, true);
        let data = std::ptr::without_provenance_mut::<u8>(0x10000);

        // SAFETY: TestPartition only records the pointer and never dereferences it.
        unsafe {
            mapper
                .map_range(&partition, None, data, (512 * MIB) as usize, 0, true, true)
                .unwrap();
        }
        assert_eq!(
            *partition.maps.lock(),
            [MapCall {
                range: MemoryRange::new(0..64 * MIB),
                data: data.addr(),
            }]
        );

        assert!(mapper.map_deferred_on_fault(&partition, 257 * MIB).unwrap());
        assert!(
            mapper
                .map_deferred_on_fault(&partition, 257 * MIB + 4096)
                .unwrap()
        );
        assert_eq!(
            *partition.maps.lock(),
            [
                MapCall {
                    range: MemoryRange::new(0..64 * MIB),
                    data: data.addr(),
                },
                MapCall {
                    range: MemoryRange::new(256 * MIB..258 * MIB),
                    data: data.wrapping_add((256 * MIB) as usize).addr(),
                },
            ]
        );

        mapper.unmap_range(&partition, 0, 512 * MIB).unwrap();
        assert_eq!(
            *partition.unmaps.lock(),
            [
                MemoryRange::new(0..64 * MIB),
                MemoryRange::new(256 * MIB..258 * MIB),
            ]
        );
    }

    #[test]
    fn lazy_registration_restores_only_registered_overlay_underlays() {
        const MIB: u64 = 1024 * 1024;
        let partition = TestPartition::default();
        let mapper = WhpMemoryMapper::with_lazy_registration(true, true);
        let data = std::ptr::without_provenance_mut::<u8>(0x10000);

        // SAFETY: TestPartition only records the pointer and never dereferences it.
        unsafe {
            mapper
                .map_range(&partition, None, data, (128 * MIB) as usize, 0, true, true)
                .unwrap();
        }

        let registered_gpa = MIB;
        let registered_overlay = Arc::new(SharedMem::new(
            Allocation::new(HV_PAGE_SIZE as usize).unwrap(),
        ));
        assert!(mapper.add_overlay_page(
            &partition,
            registered_gpa,
            registered_overlay,
            false,
            false,
        ));
        mapper.remove_overlay_page(&partition, registered_gpa);
        assert_eq!(
            partition.maps.lock().last().copied(),
            Some(MapCall {
                range: MemoryRange::new(registered_gpa..registered_gpa + HV_PAGE_SIZE),
                data: data.wrapping_add(registered_gpa as usize).addr(),
            })
        );

        let deferred_gpa = 100 * MIB;
        let deferred_overlay = Arc::new(SharedMem::new(
            Allocation::new(HV_PAGE_SIZE as usize).unwrap(),
        ));
        assert!(mapper.add_overlay_page(&partition, deferred_gpa, deferred_overlay, false, false,));
        mapper.remove_overlay_page(&partition, deferred_gpa);
        assert_eq!(
            partition.unmaps.lock().last().copied(),
            Some(MemoryRange::new(deferred_gpa..deferred_gpa + HV_PAGE_SIZE))
        );
    }
}
