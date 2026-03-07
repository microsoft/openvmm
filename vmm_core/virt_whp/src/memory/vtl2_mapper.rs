// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a partition memory mapper that implements deferred mapping and
//! VTL protections.
//!
//! For now this is limited to WHP due to the remote mapping type being a
//! Windows process, but the intention is that this code could be moved to a new
//! crate and used for any underlying virt implementation.

use super::MemoryMapper;
use super::SimpleMemoryMap;
use super::VtlAccess;
use anyhow::Context;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use range_map_vec::RangeMap;
use sparse_mmap::AsMappableRef;
use std::cmp::max;
use std::cmp::min;
use std::ops::Deref;
use std::ops::DerefMut;
use std::ops::RangeInclusive;
use std::os::windows::prelude::BorrowedHandle;
use std::os::windows::prelude::OwnedHandle;
use std::sync::Arc;
use virt::PageVisibility;

/// Mapping state for managing a partition's map_range calls.
#[derive(Debug, Inspect)]
#[inspect(external_tag)]
pub enum MappingState {
    /// Defer mapping memory unless it is contained within a range in
    /// `allowed_ranges`.
    Deferred {
        #[inspect(iter_by_index)]
        allowed_ranges: Vec<MemoryRange>,
        #[inspect(iter_by_index)]
        deferred: Vec<DeferredMapping>,
        /// Tracks all map_ranges that were called to allow unwinding VTL
        /// protections on reset.
        #[inspect(with = "inspect_helpers::inspect_range_map")]
        mapped_ranges: RangeMap<u64, DeferredMapping>,
    },
    /// Internal state used only inside functions to implement state changes.
    StateChanging,
    /// Map all ranges with WHP.
    Mapped {
        /// The MappingState to transition to on reset.
        reset_state: ResetMappingState,
        /// All calls to map_ranges.
        #[inspect(with = "inspect_helpers::inspect_range_map")]
        mapped_ranges: RangeMap<u64, DeferredMapping>,
    },
    /// Supports page acceptance by emulating isolation. While this emulates
    /// isolation at the partition mapping level, devices can still DMA to
    /// unaccepted pages via [`guestmemory`]. Additionally, VP state is
    /// not isolated from the host.
    ///
    /// The main purpose of this mode is to help test isolated guests for local
    /// dev and CI, since VBS and SNP are not yet supported for real in any
    /// OpenVMM virt implementations yet.
    EmulatedIsolation {
        /// Current visibility of addresses. A page in this map is accepted.
        #[inspect(with = "inspect_helpers::inspect_range_map")]
        current_vis: RangeMap<u64, PageVisibility>,
        /// Tracks all map_range calls.
        #[inspect(with = "inspect_helpers::inspect_range_map")]
        mapped_ranges: RangeMap<u64, DeferredMapping>,
    },
}

/// The mapping state that should be set on reset.
#[derive(Debug, Inspect)]
#[inspect(external_tag)]
pub enum ResetMappingState {
    Deferred {
        #[inspect(iter_by_index)]
        allowed_ranges: Vec<MemoryRange>,
    },
    Mapped,
}

mod inspect_helpers {
    use super::*;

    pub fn inspect_range_map<T: Inspect>(range_map: &RangeMap<u64, T>) -> impl Inspect + '_ {
        inspect::iter_by_key(range_map.iter().map(|(range, inner)| {
            (
                format!("{:010x}-{:010x}", range.start(), range.end() + 1),
                inner,
            )
        }))
    }
}

/// Struct tracking map_range calls.
#[derive(Debug, Inspect)]
pub struct DeferredMapping {
    #[inspect(skip)]
    process: Option<OwnedHandle>,
    #[inspect(debug)]
    data: *mut u8,
    #[inspect(hex)]
    size: usize,
    #[inspect(hex)]
    addr: u64,
    writable: bool,
    exec: bool,
}

// SAFETY: data is Send because the raw pointer is safe to send to different
// threads.
unsafe impl Send for DeferredMapping {}
// SAFETY: data is Sync because the raw pointer is safe to use on different
// threads.
unsafe impl Sync for DeferredMapping {}

#[derive(Debug, Inspect)]
pub(crate) struct VtlMemoryMapper {
    mapping_state: Mutex<MappingState>,
}

impl VtlMemoryMapper {
    pub fn new(mapping_state: MappingState) -> Self {
        Self {
            mapping_state: Mutex::new(mapping_state),
        }
    }

    unsafe fn map_deferred_internal(
        &self,
        partition: &dyn SimpleMemoryMap,
        mapping: &DeferredMapping,
    ) -> anyhow::Result<()> {
        let DeferredMapping {
            process,
            data,
            size,
            addr,
            writable,
            exec,
        } = mapping;
        tracing::trace!(
            ?process,
            ?data,
            ?size,
            ?addr,
            ?writable,
            ?exec,
            "map_deferred_internal"
        );
        let process = process.as_ref().map(|owned| owned.as_handle());
        // SAFETY: The caller must guarantee these values are a valid mapping
        // call.
        unsafe { partition.map_range(process, *data, *size, *addr, *writable, *exec) }
    }
}

/// Map a subset of this range allowing any ranges that overlap with
/// `allowed_ranges`. Returns a [`RangeMap`] representing ranges that were part
/// of this mapping call that were not mapped.
fn map_subset(
    partition: &dyn SimpleMemoryMap,
    process: Option<BorrowedHandle<'_>>,
    data: *mut u8,
    size: usize,
    addr: u64,
    writable: bool,
    exec: bool,
    allowed_ranges: impl Iterator<Item = RangeInclusive<u64>>,
) -> anyhow::Result<RangeMap<u64, ()>> {
    let total_range_inclusive = addr..=(addr + size as u64 - 1);
    let mut unmapped_ranges: RangeMap<u64, ()> = RangeMap::new();
    unmapped_ranges.insert(total_range_inclusive, ());

    // Every currently accepted range is allowed to actually map
    for allowed in allowed_ranges {
        let overlaps = unmapped_ranges.remove_range(allowed.clone());
        for (removed_start, removed_end, _) in overlaps {
            // Make the accepted end and removed end addr exclusive, as
            // range map is inclusive.
            let removed_end = removed_end + 1;
            // Map the range allowed based on current page acceptance.
            let allowed_base = max(*allowed.start(), removed_start);
            let allowed_end = min(*allowed.end() + 1, removed_end);

            let offset = (allowed_base - addr) as usize;
            let len = (allowed_end - allowed_base) as usize;
            // SAFETY: We assert that this range is within the
            // original map call, by validating that offset and len
            // are within the original arguments.
            unsafe {
                assert!(offset < size);
                assert!(len <= (size - offset));
                let allowed_data = data.add(offset);

                tracing::trace!(allowed_base, len, "mapping allowed range");
                partition.map_range(process, allowed_data, len, allowed_base, writable, exec)?
            }

            if removed_start < allowed_base {
                let base = removed_start;
                let end = allowed_base - 1;
                assert!(unmapped_ranges.insert(base..=end, ()));
            }

            if removed_end > allowed_end {
                let base = allowed_end;
                let end: u64 = removed_end - 1;
                assert!(unmapped_ranges.insert(base..=end, ()));
            }
        }
    }

    Ok(unmapped_ranges)
}

impl MemoryMapper for VtlMemoryMapper {
    unsafe fn map_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        process: Option<BorrowedHandle<'_>>,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        exec: bool,
    ) -> anyhow::Result<()> {
        let mut state = self.mapping_state.lock();
        let total_range_inclusive = addr..=(addr + size as u64 - 1);

        match state.deref_mut() {
            MappingState::Deferred {
                allowed_ranges,
                deferred,
                mapped_ranges,
            } => {
                let to_defer = map_subset(
                    partition,
                    process,
                    data,
                    size,
                    addr,
                    writable,
                    exec,
                    allowed_ranges
                        .iter()
                        .map(|range| range.start()..=(range.end() - 1)),
                )?;

                for (defer, _) in to_defer.iter() {
                    // Calculate the subset of the original mapping call.
                    let offset = (*defer.start() - addr) as usize;
                    let len = (defer.end() - defer.start() + 1) as usize;
                    tracing::trace!(addr, offset, len, "deferring mapping");

                    // SAFETY: We assert that this deferred range is within the
                    // original map call, by validating that offset and len are
                    // within the original arguments.
                    let deferred_data = unsafe {
                        assert!(offset < size);
                        assert!(len <= (size - offset));
                        data.add(offset)
                    };

                    // TODO: Since we keep map calls in order and do not support
                    // unmap calls while deferred, this correctly handles later
                    // map calls that overwrite a previous map call.
                    //
                    // However this is a bit ineffecient, as we could just track
                    // the final state using a range_map, but that requires more
                    // interval tracking & splitting code here. Do the simple
                    // thing since perf isn't that important for this
                    // implementation at the moment.
                    deferred.push(DeferredMapping {
                        process: process.map(|handle| {
                            handle
                                .try_clone_to_owned()
                                .expect("must be able to clone handle")
                        }),
                        data: deferred_data,
                        size: len,
                        addr: *defer.start(),
                        writable,
                        exec,
                    });
                }

                // Remove existing ranges that overlap with this new mapped
                // range, and shrink them.
                if mapped_ranges
                    .get_range(total_range_inclusive.clone())
                    .is_some()
                {
                    panic!("splitting not implemented")
                }

                // Track the new range as mapping the address space
                assert!(mapped_ranges.insert(
                    total_range_inclusive,
                    DeferredMapping {
                        process: process.map(|handle| {
                            handle
                                .try_clone_to_owned()
                                .expect("must be able to clone handle")
                        }),
                        data,
                        size,
                        addr,
                        writable,
                        exec,
                    }
                ));
            }
            MappingState::Mapped {
                mapped_ranges,
                reset_state: _,
            } => {
                // SAFETY: caller guarantees `data` is a valid pointer
                // describing `size` bytes until this range is unmapped.
                unsafe {
                    partition.map_range(process, data, size, addr, writable, exec)?;
                }

                // Remove existing ranges that overlap with this new mapped
                // range, and shrink them.
                if mapped_ranges
                    .get_range(total_range_inclusive.clone())
                    .is_some()
                {
                    todo!("splitting not implemented")
                }

                // Track the new range as mapping the address space
                assert!(mapped_ranges.insert(
                    total_range_inclusive,
                    DeferredMapping {
                        process: process.map(|handle| {
                            handle
                                .try_clone_to_owned()
                                .expect("must be able to clone handle")
                        }),
                        data,
                        size,
                        addr,
                        writable,
                        exec,
                    }
                ));
            }
            MappingState::EmulatedIsolation {
                current_vis,
                mapped_ranges,
            } => {
                let total_range_inclusive = addr..=(addr + size as u64 - 1);

                map_subset(
                    partition,
                    process,
                    data,
                    size,
                    addr,
                    writable,
                    exec,
                    current_vis.iter().map(|(range, _vis)| range),
                )?;

                // Track the new range as mapping the address space, for future
                // acceptance calls.
                assert!(mapped_ranges.insert(
                    total_range_inclusive,
                    DeferredMapping {
                        process: process.map(|handle| {
                            handle
                                .try_clone_to_owned()
                                .expect("must be able to clone handle")
                        }),
                        data,
                        size,
                        addr,
                        writable,
                        exec,
                    }
                ));
            }
            MappingState::StateChanging => unreachable!(),
        }

        Ok(())
    }

    fn unmap_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        addr: u64,
        size: u64,
    ) -> anyhow::Result<()> {
        let mut state = self.mapping_state.lock();
        let mapped_ranges = match state.deref_mut() {
            MappingState::Deferred { .. } => {
                // TODO: need to split ranges to implement, but nothing calls
                // unmap_range while deferred yet.
                unimplemented!("unable to handle unmap request while mappings deferred")
            }
            MappingState::Mapped {
                mapped_ranges,
                reset_state: _,
            } => mapped_ranges,
            MappingState::EmulatedIsolation {
                current_vis,
                mapped_ranges,
            } => {
                // TODO: Normally, a well behaved host would not unmap accepted
                // ranges, as that would be a host bug or host to guest attack
                // that the hypervisor would inject the appropriate intercept.
                //
                // However, these calls occur during VMM teardown, so allow it
                // for now with a log message since it requires further
                // rearchitecting OpenVMM to support this correctly.
                if let Some((start, end, vis)) =
                    current_vis.get_range_entry(addr..=(addr + size - 1))
                {
                    tracing::warn!(
                        "unmapping accepted range with start 0x{start:x} end 0x{end:x} vis {vis:?}"
                    );
                }

                mapped_ranges
            }
            MappingState::StateChanging => unreachable!(),
        };

        partition.unmap_range(addr, size)?;

        tracing::trace!(addr, size, "unmap_range");

        // TODO: It's unclear if the hypervisor normally allows unmap
        // while a range is VTL protected. This implementation allows
        // this, though.

        // Walk each mapped range and remove any overlap with ths unmap
        // call.
        let unmap_start = addr;
        let unmap_end = addr + size - 1;
        let total_range_inclusive = unmap_start..=unmap_end;
        let overlaps = mapped_ranges.remove_range(total_range_inclusive);

        // The caller is only supposed to remove ranges in their
        // entirety, so double check that here.
        for (start, end, _) in overlaps.into_iter() {
            assert!(unmap_start <= start && unmap_end >= end);
        }

        Ok(())
    }

    fn overlays_supported(&self) -> bool {
        false
    }

    fn add_overlay_page(
        &self,
        _partition: &dyn SimpleMemoryMap,
        _gpa: u64,
        _mem: Arc<sparse_mmap::alloc::SharedMem>,
        _writable: bool,
        _executable: bool,
    ) -> bool {
        unimplemented!()
    }

    fn remove_overlay_page(&self, _partition: &dyn SimpleMemoryMap, _gpa: u64) {
        unimplemented!()
    }

    fn add_allowed_range(&self, range: MemoryRange) {
        match self.mapping_state.lock().deref_mut() {
            MappingState::Deferred { allowed_ranges, .. } => {
                allowed_ranges.push(range);
            }
            MappingState::StateChanging => unreachable!(),
            MappingState::Mapped { reset_state, .. } => match reset_state {
                ResetMappingState::Deferred { allowed_ranges } => allowed_ranges.push(range),
                ResetMappingState::Mapped => {}
            },
            MappingState::EmulatedIsolation { .. } => {}
        }
    }

    fn in_deferred_range(&self, gpa: u64) -> bool {
        match self.mapping_state.lock().deref() {
            MappingState::Deferred {
                allowed_ranges: _,
                deferred,
                mapped_ranges: _,
            } => {
                if let Some(range) = deferred
                    .iter()
                    .find(|range| gpa >= range.addr && gpa <= (range.addr + range.size as u64))
                {
                    tracing::trace!(gpa, ?range, "in deferred range");
                    true
                } else {
                    false
                }
            }
            MappingState::Mapped { .. } | MappingState::EmulatedIsolation { .. } => false,
            MappingState::StateChanging => unreachable!(),
        }
    }

    fn map_deferred(&self, partition: &dyn SimpleMemoryMap) -> anyhow::Result<()> {
        let mut state = self.mapping_state.lock();
        let prev_state = std::mem::replace(state.deref_mut(), MappingState::StateChanging);

        let new_state = match prev_state {
            MappingState::Deferred {
                allowed_ranges,
                deferred,
                mapped_ranges,
            } => {
                for mapping in deferred {
                    // SAFETY: These values come from a previous call to
                    // map_range, where a caller guaranteed `data` is a valid
                    // pointer describing `size` bytes until this range is
                    // unmapped. Because we don't support unmap while ranges are
                    // deferred, these values are valid to map.
                    unsafe {
                        self.map_deferred_internal(partition, &mapping)?;
                    }
                }

                MappingState::Mapped {
                    reset_state: ResetMappingState::Deferred { allowed_ranges },
                    mapped_ranges,
                }
            }
            MappingState::Mapped {
                mapped_ranges,
                reset_state,
            } => {
                tracing::trace!("map_deferred called while already mapped");
                MappingState::Mapped {
                    mapped_ranges,
                    reset_state,
                }
            }
            MappingState::StateChanging => unreachable!(),
            MappingState::EmulatedIsolation { .. } => prev_state,
        };

        *state = new_state;

        Ok(())
    }

    fn apply_vtl_protection(
        &self,
        partition: &dyn SimpleMemoryMap,
        addr: u64,
        size: u64,
        access: VtlAccess,
    ) -> anyhow::Result<()> {
        let mut state = self.mapping_state.lock();

        let mapped_ranges = match state.deref_mut() {
            MappingState::Deferred { .. } => {
                panic!("vtl protections not allowed in deferred state");
            }
            MappingState::StateChanging => unreachable!(),
            MappingState::Mapped {
                reset_state: _,
                mapped_ranges,
            } => mapped_ranges,
            MappingState::EmulatedIsolation {
                current_vis,
                mapped_ranges,
            } => {
                // Check that this range is accepted.
                // BUGBUG: verify this is required by the hypervisor
                let mut covered_size = size;
                let mut addr = addr;

                while covered_size != 0 {
                    match current_vis.get_entry(&addr) {
                        Some((_start, end, _vis)) => {
                            let covered_end_inclusive = addr + covered_size - 1;
                            let accepted_size = min(covered_end_inclusive, *end) - addr + 1;
                            covered_size -= accepted_size;
                            addr += accepted_size;
                        }
                        None => anyhow::bail!(
                            "cannot apply vtl protections to unaccepted range starting at 0x{addr:x} with len 0x{covered_size:x}"
                        ),
                    }
                }

                mapped_ranges
            }
        };

        match access {
            VtlAccess::NoAccess => {
                // NOTE: This does not call self.unmap_range on purpose, as
                // VTL protection unmaps do not trigger internal state
                // updates, since the underlying mapping should still be
                // there on reset.
                //
                // TODO-KVM: This wouldn't work on KVM, since you're
                // required to unmap the whole range unlike whp. One
                // implementation option would be to lookup the mapped_range
                // corresponding to this protection call, unmap that whole
                // range, then remap them in split chunks.
                partition.unmap_range(addr, size)?;
            }
            VtlAccess::ReadOnly | VtlAccess::FullAccess => {
                let size = size as usize;
                let mut mapped = 0;

                // Loop until the whole range is mapped with the specified
                // access. This is necessary since the range described by
                // the apply_vtl_protection call may cross multiple
                // map_range ranges.
                while mapped != size {
                    let base_addr = addr + mapped as u64;
                    let size = size - mapped;
                    let range = mapped_ranges
                        .get(&base_addr)
                        .context("supplied addr is not ram")?;

                    let offset = (base_addr - range.addr) as usize;
                    let mapping_size_remaining = range.size - offset;
                    let mapping_size = min(mapping_size_remaining, size);

                    let (writeable, exec) = match access {
                        VtlAccess::NoAccess => unreachable!(),
                        VtlAccess::ReadOnly => (false, false),
                        VtlAccess::FullAccess => (range.writable, range.exec),
                    };

                    // SAFETY: The call to map_range is using values from a
                    // previous call to map_range, which the caller should
                    // have passed valid arguments. The offset and len is
                    // checked to be valid within the previous call.
                    unsafe {
                        assert!(offset < range.size);
                        assert!(mapping_size <= (range.size - offset));
                        let data = range.data.add(offset);
                        let process = range.process.as_ref().map(|owned| owned.as_handle());

                        partition.map_range(
                            process,
                            data,
                            mapping_size,
                            base_addr,
                            writeable,
                            exec,
                        )?;
                    }

                    mapped += mapping_size;
                }
            }
        }

        Ok(())
    }

    fn reset_mappings(&self, partition: &dyn SimpleMemoryMap) -> anyhow::Result<()> {
        let mut state = self.mapping_state.lock();
        let prev_state = std::mem::replace(state.deref_mut(), MappingState::StateChanging);

        let (new_state, replay_mappings) = match prev_state {
            MappingState::Deferred {
                allowed_ranges,
                deferred,
                mapped_ranges,
            } => {
                // No work to do if mappings were not committed yet.
                (
                    MappingState::Deferred {
                        allowed_ranges,
                        deferred,
                        mapped_ranges,
                    },
                    Vec::new(),
                )
            }
            MappingState::Mapped {
                reset_state,
                mapped_ranges,
            } => {
                match reset_state {
                    ResetMappingState::Deferred { allowed_ranges } => {
                        // Unmap every range in mapped_ranges, to clear out what
                        // might have been mapped before.
                        for (range, _) in mapped_ranges.iter() {
                            partition
                                .unmap_range(*range.start(), range.end() - range.start() + 1)?;
                        }

                        // Replay every mapping with the previously deferred
                        // ranges. This places the partition in the same state
                        // as when reset.
                        (
                            MappingState::Deferred {
                                allowed_ranges,
                                deferred: Vec::new(),
                                mapped_ranges: RangeMap::new(),
                            },
                            mapped_ranges.into_vec(),
                        )
                    }
                    ResetMappingState::Mapped => {
                        // No previously deferred allowed ranges means that this
                        // mapping should be in the mapped state at reset.
                        (
                            MappingState::Mapped {
                                reset_state,
                                mapped_ranges: RangeMap::new(),
                            },
                            mapped_ranges.into_vec(),
                        )
                    }
                }
            }
            MappingState::EmulatedIsolation {
                current_vis: _,
                mapped_ranges,
            } => {
                // Unmap every range in mapped_ranges to clear acceptance and VTL
                // protection state.
                for (range, _) in mapped_ranges.iter() {
                    partition.unmap_range(*range.start(), range.end() - range.start() + 1)?;
                }

                // NOTE: The loader is called again to accept ranges on behalf
                // of the guest, so clear out accepted ranges back to nothing.
                (
                    MappingState::EmulatedIsolation {
                        current_vis: RangeMap::new(),
                        mapped_ranges,
                    },
                    Vec::new(),
                )
            }
            MappingState::StateChanging => unreachable!(),
        };

        *state = new_state;
        drop(state);
        tracing::trace!(?replay_mappings, "replaying mappings");

        for (
            _start,
            _end,
            DeferredMapping {
                process,
                data,
                size,
                addr,
                writable,
                exec,
            },
        ) in replay_mappings
        {
            let process = process.as_ref().map(|owned| owned.as_handle());
            tracing::trace!(addr, size, "replaying mapping");
            // SAFETY: Each DeferredMapping struct was constructed from a
            // previous call to map_range, which the caller should have
            // validated the required invariants.
            unsafe {
                self.map_range(partition, process, data, size, addr, writable, exec)?;
            }
        }

        Ok(())
    }

    fn page_acceptance_supported(&self) -> bool {
        match self.mapping_state.lock().deref() {
            MappingState::Deferred { .. } | MappingState::Mapped { .. } => false,
            MappingState::StateChanging => unreachable!(),
            MappingState::EmulatedIsolation { .. } => true,
        }
    }

    fn gpa_visibility(&self, gpa: u64) -> Option<PageVisibility> {
        match self.mapping_state.lock().deref() {
            MappingState::Deferred { .. } | MappingState::Mapped { .. } => panic!("unsupported"),
            MappingState::StateChanging => unreachable!(),
            MappingState::EmulatedIsolation {
                current_vis,
                mapped_ranges: _,
            } => current_vis.get(&gpa).cloned(),
        }
    }

    fn accept_range(
        &self,
        partition: &dyn SimpleMemoryMap,
        range: &MemoryRange,
        visibility: PageVisibility,
    ) -> anyhow::Result<()> {
        match self.mapping_state.lock().deref_mut() {
            MappingState::Deferred { .. } | MappingState::Mapped { .. } => unimplemented!(),
            MappingState::StateChanging => unreachable!(),
            MappingState::EmulatedIsolation {
                current_vis,
                mapped_ranges,
            } => {
                let total_range_inclusive = range.start()..=(range.end() - 1);

                if let Some((overlap_start, overlap_end, _)) =
                    current_vis.get_range_entry(total_range_inclusive.clone())
                {
                    anyhow::bail!(
                        "range {range} overlaps with already accepted range start 0x{overlap_start:x} end 0x{overlap_end:x}"
                    );
                }

                let overlapping_ranges = mapped_ranges.remove_range(total_range_inclusive.clone());
                let mut uncovered_range = total_range_inclusive.clone();

                for (start, end, mapping) in overlapping_ranges {
                    // Make the accepted end and removed end addr exclusive, as
                    // range map is inclusive.
                    let end_exclusive = end + 1;
                    // Map the range allowed based on current page acceptance.
                    let mapping_base = max(*total_range_inclusive.start(), start);
                    let mapping_end = min(*total_range_inclusive.end() + 1, end_exclusive);

                    let offset = (mapping_base - start) as usize;
                    let len = (mapping_end - mapping_base) as usize;
                    // SAFETY: We assert that this range is within the
                    // original map call, by validating that offset and len
                    // are within the original arguments.
                    unsafe {
                        assert!(offset < mapping.size);
                        assert!(len <= (mapping.size - offset));
                        let mapping_data = mapping.data.add(offset);

                        tracing::trace!(offset, mapping_base, len, "accepting range");
                        let process = mapping.process.as_ref().map(|owned| owned.as_handle());
                        partition.map_range(
                            process,
                            mapping_data,
                            len,
                            mapping_base,
                            mapping.writable,
                            mapping.exec,
                        )?
                    }

                    // Re-add the original removed mapping range.
                    assert!(mapped_ranges.insert(start..=end, mapping));

                    if mapping_base != *uncovered_range.start() {
                        anyhow::bail!(
                            "accepted range not covered by ram at addr {:x}",
                            uncovered_range.start()
                        );
                    }

                    uncovered_range = mapping_end..=*uncovered_range.end();
                }

                if !uncovered_range.is_empty() {
                    anyhow::bail!(
                        "accepted range not covered by ram at addr {:x}",
                        uncovered_range.start()
                    );
                }

                // Track the new visibility of this range. Merge to help keep
                // inspection results sane.
                assert!(current_vis.insert(total_range_inclusive, visibility));
                current_vis.merge_adjacent(range_map_vec::u64_is_adjacent);

                Ok(())
            }
        }
    }

    fn modify_visibility(
        &self,
        _partition: &dyn SimpleMemoryMap,
        range: &MemoryRange,
        visibility: PageVisibility,
    ) -> anyhow::Result<()> {
        match self.mapping_state.lock().deref_mut() {
            MappingState::Deferred { .. } | MappingState::Mapped { .. } => unimplemented!(),
            MappingState::StateChanging => unreachable!(),
            MappingState::EmulatedIsolation {
                current_vis,
                mapped_ranges: _,
            } => {
                let total_range_inclusive = range.start()..=(range.end() - 1);
                let overlapping = current_vis.remove_range(total_range_inclusive.clone());

                for (start, end, vis) in overlapping {
                    if start < *total_range_inclusive.start() {
                        assert!(current_vis.insert(start..=total_range_inclusive.start() - 1, vis));
                    }

                    if end > *total_range_inclusive.end() {
                        assert!(current_vis.insert(total_range_inclusive.end() + 1..=end, vis));
                    }
                }

                assert!(current_vis.insert(total_range_inclusive, visibility));

                current_vis.merge_adjacent(range_map_vec::u64_is_adjacent);
            }
        }

        Ok(())
    }
}
