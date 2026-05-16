// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VM address-space layout allocator.
//!
//! This module provides a pure-math layout allocator that places reserved and
//! fixed ranges, 32-bit MMIO, ordinary RAM, 64-bit MMIO, and post-MMIO ranges in
//! a flat guest physical address map. It has no knowledge of specific
//! architectures, firmware types, or chipset conventions; callers express those
//! policies as reserved/fixed ranges and dynamic requests.
//!
//! # Usage
//!
//! ```
//! use memory_range::MemoryRange;
//! use vm_topology::layout::{LayoutBuilder, Placement};
//!
//! let mut ram = Vec::new();
//! let mut vmbus = MemoryRange::EMPTY;
//!
//! let mut builder = LayoutBuilder::new();
//! builder.fixed(
//!     "reserved",
//!     MemoryRange::new(0xFE00_0000..0x1_0000_0000),
//! );
//! builder.request(
//!     "vmbus",
//!     &mut vmbus,
//!     128 * 1024 * 1024,
//!     1024 * 1024,
//!     Placement::Mmio32,
//! );
//! builder.ram("ram", &mut ram, 2 * 1024 * 1024 * 1024, 4096);
//!
//! let sorted = builder.allocate().unwrap();
//! assert_eq!(ram, [MemoryRange::new(0..0x8000_0000)]);
//! assert_eq!(vmbus.end(), 0xFE00_0000);
//! assert_eq!(sorted.len(), 3);
//! ```

use memory_range::MemoryRange;
use thiserror::Error;

const PAGE_SIZE: u64 = 4096;
const FOUR_GIB: u64 = 0x1_0000_0000;
const ADDRESS_LIMIT: u64 = MemoryRange::MAX_ADDRESS;

/// The placement class for a dynamic single-range layout request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// The allocation must fit below the 4 GiB boundary and is placed top down.
    Mmio32,
    /// The allocation is placed bottom up from the end of RAM.
    Mmio64,
    /// The allocation is placed bottom up after RAM and all MMIO allocations.
    ///
    /// Post-MMIO requests are allocated in caller order, not sorted by size or
    /// alignment, so they can be used for private implementation ranges that
    /// must not perturb the guest-visible RAM/MMIO layout.
    PostMmio,
}

/// The kind of a produced allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacedRangeKind {
    /// A reserved range supplied by the caller.
    Reserved,
    /// A fixed allocation supplied by the caller.
    Fixed,
    /// A 32-bit MMIO allocation.
    Mmio32,
    /// An ordinary RAM allocation.
    Ram,
    /// A 64-bit MMIO allocation.
    Mmio64,
    /// A post-MMIO allocation.
    PostMmio,
}

/// Allocation phase reported in [`AllocateError::Exhausted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllocationPhase {
    /// 32-bit MMIO placement.
    Mmio32,
    /// RAM placement.
    Ram,
    /// 64-bit MMIO placement.
    Mmio64,
    /// Post-MMIO placement.
    PostMmio,
}

/// A placed range returned by [`LayoutBuilder::allocate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedRange {
    /// The caller-supplied tag for the request.
    pub tag: String,
    /// The kind of allocation.
    pub kind: PlacedRangeKind,
    /// The placed range.
    pub range: MemoryRange,
}

/// A builder for computing a deterministic VM address-space layout.
pub struct LayoutBuilder<'a> {
    reserved: Vec<ReservedRequest>,
    fixed: Vec<FixedRequest>,
    mmio32: Vec<DynamicRequest<'a>>,
    ram: Vec<RamRequest<'a>>,
    mmio64: Vec<DynamicRequest<'a>>,
    post_mmio: Vec<DynamicRequest<'a>>,
}

struct ReservedRequest {
    tag: String,
    range: MemoryRange,
}

struct FixedRequest {
    tag: String,
    range: MemoryRange,
}

struct DynamicRequest<'a> {
    tag: String,
    target: &'a mut MemoryRange,
    size: u64,
    alignment: u64,
}

struct RamRequest<'a> {
    tag: String,
    target: &'a mut Vec<MemoryRange>,
    size: u64,
    alignment: u64,
}

trait RequestDetails {
    fn tag(&self) -> &str;
    fn size(&self) -> u64;
    fn alignment(&self) -> u64;
}

impl RequestDetails for DynamicRequest<'_> {
    fn tag(&self) -> &str {
        &self.tag
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn alignment(&self) -> u64 {
        self.alignment
    }
}

impl RequestDetails for RamRequest<'_> {
    fn tag(&self) -> &str {
        &self.tag
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn alignment(&self) -> u64 {
        self.alignment
    }
}

struct AllocationState {
    // Sorted, non-overlapping ranges not yet consumed by any request. Keeping
    // free space as the primary state lets each phase update the map
    // incrementally instead of repeatedly subtracting all allocations from the
    // whole address space.
    free: Vec<MemoryRange>,
    allocations: Vec<PlacedRange>,
    // Highest end address of ordinary RAM. High MMIO starts here so the layout
    // top is driven by requested topology rather than a caller-provided high
    // MMIO bucket size or host physical-address width.
    ram_end: u64,
}

impl AllocationState {
    fn new() -> Self {
        Self {
            free: vec![MemoryRange::new(0..ADDRESS_LIMIT)],
            allocations: Vec::new(),
            ram_end: 0,
        }
    }

    fn place_fixed(&mut self, requests: &[FixedRequest]) -> Result<(), AllocateError> {
        for request in requests {
            self.allocate_range(&request.tag, PlacedRangeKind::Fixed, request.range);
        }

        Ok(())
    }

    fn place_reserved(&mut self, requests: &[ReservedRequest]) {
        for request in requests {
            self.allocate_range(&request.tag, PlacedRangeKind::Reserved, request.range);
        }
    }

    fn place_mmio32(&mut self, requests: &mut [DynamicRequest<'_>]) -> Result<(), AllocateError> {
        // Pack 32-bit MMIO from the top of the 4 GiB window downward so RAM can
        // start at GPA 0 and grow upward through the lowest remaining space.
        // Alignment/size ordering keeps large, constrained windows from being
        // fragmented by small devices. `sort_by` is stable, so otherwise equal
        // requests keep caller order.
        requests.sort_by(|request, other_request| {
            other_request
                .alignment
                .cmp(&request.alignment)
                .then(other_request.size.cmp(&request.size))
        });

        for request in requests {
            let Some(start) =
                find_highest_fit(&self.free, request.size, request.alignment, 0, FOUR_GIB)
            else {
                return Err(exhausted_error(
                    request,
                    AllocationPhase::Mmio32,
                    &self.free,
                    0,
                    FOUR_GIB,
                ));
            };

            let range = MemoryRange::new(start..start + request.size);
            *request.target = range;
            self.allocate_range(&request.tag, PlacedRangeKind::Mmio32, range);
        }

        Ok(())
    }

    fn place_ram(&mut self, requests: &mut [RamRequest<'_>]) -> Result<(), AllocateError> {
        // Ordinary RAM is the only splittable request type in this API. It is
        // placed after low MMIO so the resulting RAM extents describe the
        // actual guest-visible memory map, including holes below 4 GiB.
        for request in requests {
            let ranges = find_lowest_splittable_fit(
                &self.free,
                request.size,
                request.alignment,
                0,
                ADDRESS_LIMIT,
            )
            .ok_or_else(|| {
                exhausted_error(request, AllocationPhase::Ram, &self.free, 0, ADDRESS_LIMIT)
            })?;

            request.target.clear();
            request.target.extend_from_slice(&ranges);
            for range in ranges {
                self.allocate_range(&request.tag, PlacedRangeKind::Ram, range);
            }
        }

        Ok(())
    }

    fn place_mmio64(&mut self, requests: &mut [DynamicRequest<'_>]) -> Result<(), AllocateError> {
        // High MMIO is allocated bottom up from the end of RAM. The allocator
        // intentionally does not take host physical-address width as an input;
        // callers validate the resulting top against host capabilities later.
        requests.sort_by(|request, other_request| {
            other_request
                .alignment
                .cmp(&request.alignment)
                .then(other_request.size.cmp(&request.size))
        });

        for request in requests {
            let Some(start) = find_lowest_fit(
                &self.free,
                request.size,
                request.alignment,
                self.ram_end,
                ADDRESS_LIMIT,
            ) else {
                return Err(exhausted_error(
                    request,
                    AllocationPhase::Mmio64,
                    &self.free,
                    self.ram_end,
                    ADDRESS_LIMIT,
                ));
            };

            let range = MemoryRange::new(start..start + request.size);
            *request.target = range;
            self.allocate_range(&request.tag, PlacedRangeKind::Mmio64, range);
        }

        Ok(())
    }

    fn place_post_mmio(
        &mut self,
        requests: &mut [DynamicRequest<'_>],
    ) -> Result<(), AllocateError> {
        // These ranges are intentionally placed after all RAM/MMIO work and in
        // caller order. They are for implementation-private ranges that should
        // not change the VTL0-visible layout or be reordered by alignment.
        for request in requests {
            let layout_top = self.layout_top();
            let Some(start) = find_lowest_fit(
                &self.free,
                request.size,
                request.alignment,
                layout_top,
                ADDRESS_LIMIT,
            ) else {
                return Err(exhausted_error(
                    request,
                    AllocationPhase::PostMmio,
                    &self.free,
                    layout_top,
                    ADDRESS_LIMIT,
                ));
            };

            let range = MemoryRange::new(start..start + request.size);
            *request.target = range;
            self.allocate_range(&request.tag, PlacedRangeKind::PostMmio, range);
        }

        Ok(())
    }

    fn layout_top(&self) -> u64 {
        self.allocations
            .iter()
            .filter(|allocation| allocation.kind != PlacedRangeKind::Reserved)
            .map(|allocation| allocation.range.end())
            .max()
            .unwrap_or(0)
    }

    fn record(&mut self, tag: &str, kind: PlacedRangeKind, range: MemoryRange) {
        self.allocations.push(PlacedRange {
            tag: tag.to_string(),
            kind,
            range,
        });

        if kind == PlacedRangeKind::Ram {
            self.ram_end = self.ram_end.max(range.end());
        }
    }

    fn allocate_range(&mut self, tag: &str, kind: PlacedRangeKind, range: MemoryRange) {
        self.remove_free_range(range);
        self.record(tag, kind, range);
    }

    fn remove_free_range(&mut self, allocated: MemoryRange) {
        let free_index = self
            .free
            .partition_point(|range| range.start() <= allocated.start())
            .checked_sub(1)
            .expect("allocated range must be contained in the free list");
        assert!(self.free[free_index].contains(&allocated));
        let free_range = self.free.remove(free_index);

        let mut insert_index = free_index;
        if free_range.start() < allocated.start() {
            self.free.insert(
                insert_index,
                MemoryRange::new(free_range.start()..allocated.start()),
            );
            insert_index += 1;
        }
        if allocated.end() < free_range.end() {
            self.free.insert(
                insert_index,
                MemoryRange::new(allocated.end()..free_range.end()),
            );
        }
    }
}

/// Error returned by [`LayoutBuilder::allocate`].
#[derive(Debug, Error)]
pub enum AllocateError {
    /// A request has an invalid size.
    #[error("{tag}: invalid size {size:#x} (must be > 0 and a multiple of {PAGE_SIZE:#x})")]
    InvalidSize {
        /// The tag identifying the request.
        tag: String,
        /// The invalid size.
        size: u64,
    },
    /// A request has an invalid alignment.
    #[error("{tag}: invalid alignment {alignment:#x} (must be >= {PAGE_SIZE:#x} and a power of 2)")]
    InvalidAlignment {
        /// The tag identifying the request.
        tag: String,
        /// The invalid alignment.
        alignment: u64,
    },
    /// Two fixed or reserved requests overlap.
    #[error("fixed/reserved requests {tag_a} ({range_a}) and {tag_b} ({range_b}) overlap")]
    FixedOverlap {
        /// The tag of the first request.
        tag_a: String,
        /// The range of the first request.
        range_a: MemoryRange,
        /// The tag of the second request.
        tag_b: String,
        /// The range of the second request.
        range_b: MemoryRange,
    },
    /// A dynamic request could not be satisfied.
    #[error(
        "{tag}: cannot allocate {size:#x} bytes with alignment {alignment:#x} during {phase:?}; remaining free space in phase: {free_space:#x} bytes"
    )]
    Exhausted {
        /// The tag identifying the request.
        tag: String,
        /// The requested size.
        size: u64,
        /// The requested alignment.
        alignment: u64,
        /// The allocation phase.
        phase: AllocationPhase,
        /// The remaining free space in the phase.
        free_space: u64,
    },
}

impl<'a> LayoutBuilder<'a> {
    /// Creates a new layout builder.
    pub fn new() -> Self {
        Self {
            reserved: Vec::new(),
            fixed: Vec::new(),
            mmio32: Vec::new(),
            ram: Vec::new(),
            mmio64: Vec::new(),
            post_mmio: Vec::new(),
        }
    }

    /// Reserves a range so no allocation can use it.
    ///
    /// Reserved ranges are removed from the free list and may appear in the
    /// returned [`PlacedRange`] list, but they do not affect post-MMIO
    /// placement. Trailing reserved ranges are omitted from the returned list.
    pub fn reserve(&mut self, tag: impl Into<String>, range: MemoryRange) {
        self.reserved.push(ReservedRequest {
            tag: tag.into(),
            range,
        });
    }

    /// Adds a fixed range request to the builder.
    ///
    pub fn fixed(&mut self, tag: impl Into<String>, range: MemoryRange) {
        self.fixed.push(FixedRequest {
            tag: tag.into(),
            range,
        });
    }

    /// Adds a dynamic single-range request to the builder.
    ///
    /// The target is filled in when [`Self::allocate`] succeeds.
    pub fn request(
        &mut self,
        tag: impl Into<String>,
        target: &'a mut MemoryRange,
        size: u64,
        alignment: u64,
        placement: Placement,
    ) {
        match placement {
            Placement::Mmio32 => self.mmio32.push(DynamicRequest {
                tag: tag.into(),
                target,
                size,
                alignment,
            }),
            Placement::Mmio64 => self.mmio64.push(DynamicRequest {
                tag: tag.into(),
                target,
                size,
                alignment,
            }),
            Placement::PostMmio => self.post_mmio.push(DynamicRequest {
                tag: tag.into(),
                target,
                size,
                alignment,
            }),
        }
    }

    /// Adds an ordinary RAM request to the builder.
    ///
    /// RAM is placed bottom up from GPA 0 and may split around fixed and MMIO32
    /// ranges. The target vector is replaced with the placed RAM extents when
    /// [`Self::allocate`] succeeds.
    pub fn ram(
        &mut self,
        tag: impl Into<String>,
        target: &'a mut Vec<MemoryRange>,
        size: u64,
        alignment: u64,
    ) {
        self.ram.push(RamRequest {
            tag: tag.into(),
            target,
            size,
            alignment,
        });
    }

    /// Allocates all requests, fills in each target, and returns every placed
    /// range sorted by address.
    pub fn allocate(mut self) -> Result<Vec<PlacedRange>, AllocateError> {
        validate_reserved_requests(&self.reserved)?;
        validate_fixed_requests(&self.fixed)?;
        validate_pinned_ranges(&self.reserved, &self.fixed)?;
        validate_dynamic_requests(&self.mmio32)?;
        validate_ram_requests(&self.ram)?;
        validate_dynamic_requests(&self.mmio64)?;
        validate_dynamic_requests(&self.post_mmio)?;

        let mut state = AllocationState::new();
        state.place_reserved(&self.reserved);
        state.place_fixed(&self.fixed)?;
        state.place_mmio32(&mut self.mmio32)?;
        state.place_ram(&mut self.ram)?;
        state.place_mmio64(&mut self.mmio64)?;
        state.place_post_mmio(&mut self.post_mmio)?;

        state.allocations.sort_by_key(|allocation| allocation.range);
        while state
            .allocations
            .last()
            .is_some_and(|allocation| allocation.kind == PlacedRangeKind::Reserved)
        {
            state.allocations.pop();
        }
        Ok(state.allocations)
    }
}

impl Default for LayoutBuilder<'_> {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_size_alignment(tag: &str, size: u64, alignment: u64) -> Result<(), AllocateError> {
    if size == 0 || !size.is_multiple_of(PAGE_SIZE) {
        return Err(AllocateError::InvalidSize {
            tag: tag.to_string(),
            size,
        });
    }

    if alignment < PAGE_SIZE || !alignment.is_power_of_two() {
        return Err(AllocateError::InvalidAlignment {
            tag: tag.to_string(),
            alignment,
        });
    }

    Ok(())
}

fn validate_reserved_requests(requests: &[ReservedRequest]) -> Result<(), AllocateError> {
    for request in requests {
        validate_size_alignment(&request.tag, request.range.len(), PAGE_SIZE)?;
    }

    Ok(())
}

fn validate_fixed_requests(requests: &[FixedRequest]) -> Result<(), AllocateError> {
    for request in requests {
        validate_size_alignment(&request.tag, request.range.len(), PAGE_SIZE)?;
    }

    Ok(())
}

fn validate_pinned_ranges(
    reserved_requests: &[ReservedRequest],
    fixed_requests: &[FixedRequest],
) -> Result<(), AllocateError> {
    let mut pinned = reserved_requests
        .iter()
        .map(|request| (request.range, request.tag.as_str()))
        .chain(
            fixed_requests
                .iter()
                .map(|request| (request.range, request.tag.as_str())),
        )
        .collect::<Vec<_>>();

    pinned.sort_by_key(|(range, _)| range.start());

    for pair in pinned.windows(2) {
        let (range_a, tag_a) = pair[0];
        let (range_b, tag_b) = pair[1];
        if range_a.overlaps(&range_b) {
            return Err(AllocateError::FixedOverlap {
                tag_a: tag_a.to_string(),
                range_a,
                tag_b: tag_b.to_string(),
                range_b,
            });
        }
    }

    Ok(())
}

fn validate_dynamic_requests(requests: &[DynamicRequest<'_>]) -> Result<(), AllocateError> {
    for request in requests {
        validate_size_alignment(&request.tag, request.size, request.alignment)?;
    }

    Ok(())
}

fn validate_ram_requests(requests: &[RamRequest<'_>]) -> Result<(), AllocateError> {
    for request in requests {
        validate_size_alignment(&request.tag, request.size, request.alignment)?;
    }

    Ok(())
}

fn exhausted_error(
    request: &impl RequestDetails,
    phase: AllocationPhase,
    free_ranges: &[MemoryRange],
    region_start: u64,
    region_end: u64,
) -> AllocateError {
    AllocateError::Exhausted {
        tag: request.tag().to_string(),
        size: request.size(),
        alignment: request.alignment(),
        phase,
        free_space: free_space_in_region(free_ranges, region_start, region_end),
    }
}

fn free_space_in_region(free_ranges: &[MemoryRange], region_start: u64, region_end: u64) -> u64 {
    free_ranges
        .iter()
        .map(|range| {
            let effective_start = range.start().max(region_start);
            let effective_end = range.end().min(region_end);
            effective_end.saturating_sub(effective_start)
        })
        .sum()
}

fn find_highest_fit(
    free_ranges: &[MemoryRange],
    size: u64,
    alignment: u64,
    region_start: u64,
    region_end: u64,
) -> Option<u64> {
    for range in free_ranges.iter().rev() {
        let effective_start = range.start().max(region_start);
        let effective_end = range.end().min(region_end);

        if effective_start >= effective_end || effective_end - effective_start < size {
            continue;
        }

        let latest_start = effective_end - size;
        let aligned_start = align_down(latest_start, alignment);
        if aligned_start >= effective_start {
            return Some(aligned_start);
        }
    }

    None
}

fn find_lowest_fit(
    free_ranges: &[MemoryRange],
    size: u64,
    alignment: u64,
    region_start: u64,
    region_end: u64,
) -> Option<u64> {
    for range in free_ranges {
        let effective_start = range.start().max(region_start);
        let effective_end = range.end().min(region_end);

        if effective_start >= effective_end {
            continue;
        }

        let Some(aligned_start) = align_up(effective_start, alignment) else {
            continue;
        };
        let Some(end) = aligned_start.checked_add(size) else {
            continue;
        };

        if end <= effective_end {
            return Some(aligned_start);
        }
    }

    None
}

fn find_lowest_splittable_fit(
    free_ranges: &[MemoryRange],
    size: u64,
    alignment: u64,
    region_start: u64,
    region_end: u64,
) -> Option<Vec<MemoryRange>> {
    let mut remaining = size;
    let mut ranges = Vec::new();

    for range in free_ranges {
        let effective_start = range.start().max(region_start);
        let effective_end = range.end().min(region_end);

        if effective_start >= effective_end {
            continue;
        }

        let Some(aligned_start) = align_up(effective_start, alignment) else {
            continue;
        };
        if aligned_start >= effective_end {
            continue;
        }

        let available = effective_end - aligned_start;
        let allocation_size = available.min(remaining);
        ranges.push(MemoryRange::new(
            aligned_start..aligned_start + allocation_size,
        ));
        remaining -= allocation_size;

        if remaining == 0 {
            return Some(ranges);
        }
    }

    None
}

fn align_down(value: u64, alignment: u64) -> u64 {
    value & !(alignment - 1)
}

fn align_up(value: u64, alignment: u64) -> Option<u64> {
    value
        .checked_add(alignment - 1)
        .map(|value| align_down(value, alignment))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn empty_input() {
        let sorted = LayoutBuilder::new().allocate().unwrap();
        assert!(sorted.is_empty());
    }

    #[test]
    fn fixed_request_is_reported() {
        let mut builder = LayoutBuilder::new();
        let range = MemoryRange::new(0xFC00_0000..0xFC40_0000);
        builder.fixed("fixed", range);

        let sorted = builder.allocate().unwrap();

        assert_eq!(sorted[0].range, range);
        assert_eq!(sorted[0].kind, PlacedRangeKind::Fixed);
    }

    #[test]
    fn fixed_overlap_rejected() {
        let mut builder = LayoutBuilder::new();
        builder.fixed("first", MemoryRange::new(0x1000..0x3000));
        builder.fixed("second", MemoryRange::new(0x2000..0x3000));

        let error = builder.allocate().unwrap_err();

        assert!(matches!(error, AllocateError::FixedOverlap { .. }));
    }

    #[test]
    fn invalid_request_rejected() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.request("zero", &mut target, 0, PAGE_SIZE, Placement::Mmio32);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidSize { .. }
        ));

        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.request("alignment", &mut target, PAGE_SIZE, KIB, Placement::Mmio32);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidAlignment { .. }
        ));
    }

    #[test]
    fn reserved_overlap_rejected() {
        let mut builder = LayoutBuilder::new();
        builder.reserve("reserved", MemoryRange::new(GIB..GIB + MIB));
        builder.fixed(
            "fixed",
            MemoryRange::new(GIB + PAGE_SIZE..GIB + PAGE_SIZE + MIB),
        );

        let error = builder.allocate().unwrap_err();

        assert!(matches!(error, AllocateError::FixedOverlap { .. }));
    }

    #[test]
    fn mmio32_uses_top_down_placement_below_4_gib() {
        let mut first = MemoryRange::EMPTY;
        let mut second = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.fixed("reserved", MemoryRange::new(0xFE00_0000..0x1_0000_0000));
        builder.request("first", &mut first, MIB, MIB, Placement::Mmio32);
        builder.request("second", &mut second, MIB, MIB, Placement::Mmio32);

        builder.allocate().unwrap();

        assert_eq!(first, MemoryRange::new(0xFDF0_0000..0xFE00_0000));
        assert_eq!(second, MemoryRange::new(0xFDE0_0000..0xFDF0_0000));
    }

    #[test]
    fn mmio32_orders_by_alignment_then_size_then_request_order() {
        let mut small = MemoryRange::EMPTY;
        let mut aligned = MemoryRange::EMPTY;
        let mut large = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.request("small", &mut small, MIB, MIB, Placement::Mmio32);
        builder.request("aligned", &mut aligned, MIB, 256 * MIB, Placement::Mmio32);
        builder.request("large", &mut large, 4 * MIB, MIB, Placement::Mmio32);

        builder.allocate().unwrap();

        assert_eq!(aligned.start() % (256 * MIB), 0);
        assert_eq!(large.len(), 4 * MIB);
        assert_eq!(small.len(), MIB);
        assert!(!aligned.overlaps(&large));
        assert!(!aligned.overlaps(&small));
        assert!(!large.overlaps(&small));
    }

    #[test]
    fn ram_starts_at_zero() {
        let mut ram = Vec::new();
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);

        let sorted = builder.allocate().unwrap();

        assert_eq!(ram, [MemoryRange::new(0..2 * GIB)]);
        assert_eq!(sorted[0].kind, PlacedRangeKind::Ram);
        assert_eq!(sorted[0].range, ram[0]);
    }

    #[test]
    fn ram_splits_around_fixed_ranges_and_mmio32() {
        let mut mmio32 = MemoryRange::EMPTY;
        let mut ram = Vec::new();
        let mut builder = LayoutBuilder::new();
        builder.fixed("fixed", MemoryRange::new(GIB..GIB + MIB));
        builder.request("mmio32", &mut mmio32, 2 * GIB, MIB, Placement::Mmio32);
        builder.ram("ram", &mut ram, 3 * GIB, PAGE_SIZE);

        builder.allocate().unwrap();

        assert_eq!(
            ram,
            [
                MemoryRange::new(0..GIB),
                MemoryRange::new(GIB + MIB..2 * GIB),
                MemoryRange::new(FOUR_GIB..FOUR_GIB + GIB + MIB),
            ]
        );
    }

    #[test]
    fn mmio64_uses_bottom_up_placement_from_end_of_ram() {
        let mut ram = Vec::new();
        let mut first = MemoryRange::EMPTY;
        let mut second = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        builder.request("first", &mut first, MIB, MIB, Placement::Mmio64);
        builder.request("second", &mut second, MIB, MIB, Placement::Mmio64);

        builder.allocate().unwrap();

        assert_eq!(first, MemoryRange::new(2 * GIB..2 * GIB + MIB));
        assert_eq!(second, MemoryRange::new(2 * GIB + MIB..2 * GIB + 2 * MIB));
    }

    #[test]
    fn mmio64_skips_fixed_ranges_above_ram() {
        let mut ram = Vec::new();
        let mut mmio64 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        builder.fixed("fixed", MemoryRange::new(2 * GIB..2 * GIB + MIB));
        builder.request("mmio64", &mut mmio64, MIB, MIB, Placement::Mmio64);

        builder.allocate().unwrap();

        assert_eq!(mmio64, MemoryRange::new(2 * GIB + MIB..2 * GIB + 2 * MIB));
    }

    #[test]
    fn post_mmio_uses_bottom_up_placement_after_all_mmio() {
        let mut ram = Vec::new();
        let mut mmio64 = MemoryRange::EMPTY;
        let mut post_mmio = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        builder.request("mmio64", &mut mmio64, MIB, MIB, Placement::Mmio64);
        builder.request("post_mmio", &mut post_mmio, MIB, MIB, Placement::PostMmio);

        builder.allocate().unwrap();

        assert_eq!(mmio64, MemoryRange::new(2 * GIB..2 * GIB + MIB));
        assert_eq!(
            post_mmio,
            MemoryRange::new(2 * GIB + MIB..2 * GIB + 2 * MIB)
        );
    }

    #[test]
    fn post_mmio_preserves_request_order() {
        let mut ram = Vec::new();
        let mut first = MemoryRange::EMPTY;
        let mut aligned = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        builder.request("first", &mut first, MIB, MIB, Placement::PostMmio);
        builder.request("aligned", &mut aligned, MIB, GIB, Placement::PostMmio);

        builder.allocate().unwrap();

        assert_eq!(first, MemoryRange::new(2 * GIB..2 * GIB + MIB));
        assert_eq!(aligned, MemoryRange::new(3 * GIB..3 * GIB + MIB));
    }

    #[test]
    fn high_reserved_range_does_not_affect_post_mmio_placement() {
        let mut ram = Vec::new();
        let mut post_mmio = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        builder.reserve(
            "high_reserved",
            MemoryRange::new(0xFD_0000_0000..0xFD_4000_0000),
        );
        builder.request("post_mmio", &mut post_mmio, MIB, MIB, Placement::PostMmio);

        let sorted = builder.allocate().unwrap();

        assert_eq!(post_mmio, MemoryRange::new(2 * GIB..2 * GIB + MIB));
        assert!(
            !sorted
                .iter()
                .any(|allocation| allocation.kind == PlacedRangeKind::Reserved)
        );
    }

    #[test]
    fn reserved_range_between_allocations_is_reported() {
        let mut ram = Vec::new();
        let mut post_mmio = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        builder.reserve("reserved", MemoryRange::new(2 * GIB..2 * GIB + MIB));
        builder.request("post_mmio", &mut post_mmio, MIB, MIB, Placement::PostMmio);

        let sorted = builder.allocate().unwrap();

        assert_eq!(
            post_mmio,
            MemoryRange::new(2 * GIB + MIB..2 * GIB + 2 * MIB)
        );
        assert!(sorted.iter().any(|allocation| {
            allocation.kind == PlacedRangeKind::Reserved
                && allocation.range == MemoryRange::new(2 * GIB..2 * GIB + MIB)
        }));
    }

    #[test]
    fn fixed_hypertransport_hole_is_regular_fixed_placement() {
        let mut ram = Vec::new();
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
        let hypertransport = MemoryRange::new(0xFD_0000_0000..0xFD_4000_0000);
        builder.fixed("amd_hypertransport_hole", hypertransport);

        let sorted = builder.allocate().unwrap();

        assert_eq!(sorted.last().unwrap().range, hypertransport);
    }

    #[test]
    fn exhaustion_reports_phase() {
        let mut mmio32 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.request(
            "too_big",
            &mut mmio32,
            4 * GIB + PAGE_SIZE,
            PAGE_SIZE,
            Placement::Mmio32,
        );
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::Exhausted {
                phase: AllocationPhase::Mmio32,
                ..
            }
        ));

        let mut ram = Vec::new();
        let mut builder = LayoutBuilder::new();
        builder.fixed("fixed", MemoryRange::new(0..ADDRESS_LIMIT));
        builder.ram("ram", &mut ram, PAGE_SIZE, PAGE_SIZE);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::Exhausted {
                phase: AllocationPhase::Ram,
                ..
            }
        ));

        let mut ram = Vec::new();
        let mut mmio64 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, PAGE_SIZE, PAGE_SIZE);
        builder.fixed("fixed", MemoryRange::new(PAGE_SIZE..ADDRESS_LIMIT));
        builder.request(
            "mmio64",
            &mut mmio64,
            PAGE_SIZE,
            PAGE_SIZE,
            Placement::Mmio64,
        );
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::Exhausted {
                phase: AllocationPhase::Mmio64,
                ..
            }
        ));
    }

    #[test]
    fn sorted_result_preserves_tags_and_kinds() {
        let mut ram = Vec::new();
        let mut mmio32 = MemoryRange::EMPTY;
        let mut mmio64 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new();
        builder.ram("ram", &mut ram, GIB, PAGE_SIZE);
        builder.request("mmio32", &mut mmio32, MIB, MIB, Placement::Mmio32);
        builder.request("mmio64", &mut mmio64, MIB, MIB, Placement::Mmio64);

        let sorted = builder.allocate().unwrap();

        assert_eq!(sorted[0].tag, "ram");
        assert_eq!(sorted[0].kind, PlacedRangeKind::Ram);
        assert_eq!(sorted[1].tag, "mmio64");
        assert_eq!(sorted[1].kind, PlacedRangeKind::Mmio64);
        assert_eq!(sorted[2].tag, "mmio32");
        assert_eq!(sorted[2].kind, PlacedRangeKind::Mmio32);
    }

    #[test]
    fn deterministic() {
        let mut previous = None;

        for _ in 0..10 {
            let mut ram = Vec::new();
            let mut vmbus_low = MemoryRange::EMPTY;
            let mut pcie_ecam = MemoryRange::EMPTY;
            let mut pcie_high = MemoryRange::EMPTY;
            let mut virtio = MemoryRange::EMPTY;
            let mut builder = LayoutBuilder::new();
            builder.ram("ram", &mut ram, 2 * GIB, PAGE_SIZE);
            builder.fixed("reserved", MemoryRange::new(0xFE00_0000..0x1_0000_0000));
            builder.request(
                "vmbus_low",
                &mut vmbus_low,
                128 * MIB,
                MIB,
                Placement::Mmio32,
            );
            builder.request(
                "pcie_ecam",
                &mut pcie_ecam,
                256 * MIB,
                256 * MIB,
                Placement::Mmio32,
            );
            builder.request("pcie_high", &mut pcie_high, GIB, MIB, Placement::Mmio64);
            builder.request(
                "virtio",
                &mut virtio,
                PAGE_SIZE,
                PAGE_SIZE,
                Placement::Mmio32,
            );

            let sorted = builder.allocate().unwrap();
            if let Some(previous) = &previous {
                assert_eq!(previous, &sorted);
            }
            previous = Some(sorted);
        }
    }
}
