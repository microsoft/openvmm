// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! MMIO layout allocator.
//!
//! This module provides a pure-math layout allocator that assigns address
//! ranges to MMIO consumers within a flat physical address space. It has no
//! knowledge of specific architectures, firmware types, or pinned-address
//! conventions — those are the responsibility of the caller (typically
//! `vm_manifest_builder`).
//!
//! # Usage
//!
//! ```
//! use memory_range::MemoryRange;
//! use vm_topology::layout::{Constraint, LayoutBuilder};
//!
//! let mut reserved = MemoryRange::EMPTY;
//! let mut vmbus = MemoryRange::EMPTY;
//! let mut pcie_bar = MemoryRange::EMPTY;
//!
//! let mut builder = LayoutBuilder::new(48);
//!
//! // Reserve a pinned range for architectural devices.
//! builder.request("reserved", &mut reserved, 32 * 1024 * 1024, 4096, Constraint::Pinned(0xFE00_0000));
//!
//! // Dynamic allocation below 4 GiB.
//! builder.request("vmbus", &mut vmbus, 128 * 1024 * 1024, 1024 * 1024, Constraint::Below4GiB);
//!
//! // Dynamic allocation above 4 GiB.
//! builder.request(
//!     "pcie_bar",
//!     &mut pcie_bar,
//!     1024 * 1024 * 1024,
//!     1024 * 1024,
//!     Constraint::Above4GiB,
//! );
//!
//! let sorted = builder.allocate().unwrap();
//! assert_eq!(reserved, MemoryRange::new(0xFE00_0000..0x1_0000_0000));
//! assert_eq!(sorted.len(), 3);
//! ```

use memory_range::MemoryRange;
use memory_range::subtract_ranges;
use thiserror::Error;

const PAGE_SIZE: u64 = 4096;
const FOUR_GIB: u64 = 0x1_0000_0000;

/// The constraint on where a layout request can be placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constraint {
    /// The allocation must fit entirely below the 4 GiB boundary.
    Below4GiB,
    /// The allocation must start at or above the 4 GiB boundary.
    Above4GiB,
    /// The allocation must be placed at exactly the given address.
    Pinned(u64),
}

/// A builder for computing an MMIO layout by collecting requests and
/// then allocating them within a physical address space.
///
/// The address space is `[0, 1 << physical_address_width)`. Consumers
/// call [`Self::request`] to declare allocation needs (passing a
/// `&mut MemoryRange` that will be filled in), then [`Self::allocate`]
/// to run the greedy placement algorithm.
pub struct LayoutBuilder<'a> {
    physical_address_width: u8,
    targets: Vec<&'a mut MemoryRange>,
    requests: Vec<RequestEntry>,
}

struct RequestEntry {
    tag: String,
    size: u64,
    alignment: u64,
    constraint: Constraint,
    input_order: usize,
}

/// Error returned by [`LayoutBuilder::allocate`].
#[derive(Debug, Error)]
pub enum AllocateError {
    /// The physical address width is invalid (must be 1..=63).
    #[error("invalid physical address width {0} (must be 1..=63)")]
    InvalidAddressWidth(u8),
    /// A request has an invalid size (must be > 0 and page-aligned).
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
    /// A pinned request has a non-page-aligned address.
    #[error("{tag}: pinned address {address:#x} is not page-aligned")]
    InvalidPinnedAddress {
        /// The tag identifying the request.
        tag: String,
        /// The invalid address.
        address: u64,
    },
    /// A pinned request extends beyond the physical address space.
    #[error("{tag}: pinned range {address:#x}..{end:#x} exceeds address space limit {limit:#x}")]
    PinnedOutOfBounds {
        /// The tag identifying the request.
        tag: String,
        /// The start address.
        address: u64,
        /// The end address.
        end: u64,
        /// The address space limit.
        limit: u64,
    },
    /// Two pinned requests overlap.
    #[error("pinned requests {tag_a} ({range_a}) and {tag_b} ({range_b}) overlap")]
    PinnedOverlap {
        /// The tag of the first pinned request.
        tag_a: String,
        /// The range of the first pinned request.
        range_a: MemoryRange,
        /// The tag of the second pinned request.
        tag_b: String,
        /// The range of the second pinned request.
        range_b: MemoryRange,
    },
    /// A dynamic request could not be satisfied.
    #[error(
        "{tag}: cannot allocate {size:#x} bytes with alignment {alignment:#x} \
         and constraint {constraint:?}; remaining free space in region: {free_space:#x} bytes"
    )]
    Exhausted {
        /// The tag identifying the request.
        tag: String,
        /// The requested size.
        size: u64,
        /// The requested alignment.
        alignment: u64,
        /// The placement constraint.
        constraint: Constraint,
        /// The remaining free space in the constrained region.
        free_space: u64,
    },
}

impl<'a> LayoutBuilder<'a> {
    /// Creates a new layout builder for the given physical address width.
    ///
    /// The address space is `[0, 1 << physical_address_width)`.
    /// `physical_address_width` must be in the range `1..=63`.
    pub fn new(physical_address_width: u8) -> Self {
        Self {
            physical_address_width,
            targets: Vec::new(),
            requests: Vec::new(),
        }
    }

    /// Adds a request to the builder.
    ///
    /// - `tag`: A descriptive name for the request (used in error messages).
    /// - `target`: A mutable reference to a [`MemoryRange`] that will be
    ///   filled in with the allocated range when [`Self::allocate`] is
    ///   called.
    /// - `size`: The size in bytes. Must be > 0 and a multiple of 4096.
    /// - `alignment`: The required alignment. Must be >= 4096 and a power
    ///   of 2.
    /// - `constraint`: Where the allocation may be placed.
    pub fn request(
        &mut self,
        tag: impl Into<String>,
        target: &'a mut MemoryRange,
        size: u64,
        alignment: u64,
        constraint: Constraint,
    ) {
        let input_order = self.requests.len();
        self.targets.push(target);
        self.requests.push(RequestEntry {
            tag: tag.into(),
            size,
            alignment,
            constraint,
            input_order,
        });
    }

    /// Allocates all requests, fills in each target `&mut MemoryRange`,
    /// and returns every allocation sorted by address.
    ///
    /// The algorithm:
    /// 1. Places all [`Constraint::Pinned`] requests at their fixed
    ///    addresses, validating no overlaps.
    /// 2. Sorts non-pinned requests by `(alignment desc, size desc,
    ///    input_order asc)`.
    /// 3. Greedy top-down placement: for each non-pinned request, finds
    ///    the highest-address position in the constrained region that
    ///    satisfies size and alignment.
    /// 4. Writes each result to its `&mut MemoryRange` target and
    ///    returns a `Vec<MemoryRange>` of all allocations sorted by
    ///    address.
    pub fn allocate(mut self) -> Result<Vec<MemoryRange>, AllocateError> {
        let width = self.physical_address_width;
        if !(1..=63).contains(&width) {
            return Err(AllocateError::InvalidAddressWidth(width));
        }
        let address_limit = 1u64 << width;

        // Validate all requests up front.
        for req in &self.requests {
            if req.size == 0 || req.size % PAGE_SIZE != 0 {
                return Err(AllocateError::InvalidSize {
                    tag: req.tag.clone(),
                    size: req.size,
                });
            }
            if req.alignment < PAGE_SIZE || !req.alignment.is_power_of_two() {
                return Err(AllocateError::InvalidAlignment {
                    tag: req.tag.clone(),
                    alignment: req.alignment,
                });
            }
            if let Constraint::Pinned(addr) = req.constraint {
                if addr % PAGE_SIZE != 0 {
                    return Err(AllocateError::InvalidPinnedAddress {
                        tag: req.tag.clone(),
                        address: addr,
                    });
                }
                let end =
                    addr.checked_add(req.size)
                        .ok_or_else(|| AllocateError::PinnedOutOfBounds {
                            tag: req.tag.clone(),
                            address: addr,
                            end: u64::MAX,
                            limit: address_limit,
                        })?;
                if end > address_limit {
                    return Err(AllocateError::PinnedOutOfBounds {
                        tag: req.tag.clone(),
                        address: addr,
                        end,
                        limit: address_limit,
                    });
                }
            }
        }

        let mut allocations: Vec<MemoryRange> = vec![MemoryRange::EMPTY; self.requests.len()];

        // Step 1: Collect pinned requests, sort by address, check for
        // overlaps with a single adjacent-pair scan.
        let mut pinned: Vec<(MemoryRange, usize)> = self
            .requests
            .iter()
            .enumerate()
            .filter_map(|(i, req)| {
                if let Constraint::Pinned(addr) = req.constraint {
                    Some((MemoryRange::new(addr..addr + req.size), i))
                } else {
                    None
                }
            })
            .collect();

        pinned.sort_by_key(|(range, _)| range.start());

        for [(range_a, idx_a), (range_b, idx_b)] in pinned.array_windows() {
            if range_a.overlaps(range_b) {
                return Err(AllocateError::PinnedOverlap {
                    tag_a: self.requests[*idx_a].tag.clone(),
                    range_a: *range_a,
                    tag_b: self.requests[*idx_b].tag.clone(),
                    range_b: *range_b,
                });
            }
        }

        for &(range, idx) in &pinned {
            allocations[idx] = range;
        }

        // Compute free space by subtracting all pinned ranges from the
        // full address space in one pass. Both inputs are sorted and
        // non-overlapping, so subtract_ranges runs in linear time.
        let pinned_ranges: Vec<MemoryRange> = pinned.iter().map(|(r, _)| *r).collect();
        let mut free_ranges: Vec<MemoryRange> = subtract_ranges(
            [MemoryRange::new(0..address_limit)],
            pinned_ranges.iter().copied(),
        )
        .collect();

        // Step 2: Collect non-Pinned request indices, sort by
        // (alignment DESC, size DESC, input_order ASC).
        let mut dynamic: Vec<usize> = self
            .requests
            .iter()
            .enumerate()
            .filter(|(_, req)| !matches!(req.constraint, Constraint::Pinned(_)))
            .map(|(i, _)| i)
            .collect();

        dynamic.sort_by(|&a, &b| {
            let ra = &self.requests[a];
            let rb = &self.requests[b];
            rb.alignment
                .cmp(&ra.alignment)
                .then(rb.size.cmp(&ra.size))
                .then(ra.input_order.cmp(&rb.input_order))
        });

        // Step 3: Greedy top-down placement. For each dynamic request,
        // reverse-scan the sorted free list for the highest-address fit,
        // then update the free list via subtract_ranges.
        for &idx in &dynamic {
            let req = &self.requests[idx];
            let (region_start, region_end) = match req.constraint {
                Constraint::Below4GiB => (0, FOUR_GIB.min(address_limit)),
                Constraint::Above4GiB => (FOUR_GIB, address_limit),
                Constraint::Pinned(_) => unreachable!(),
            };

            match find_highest_fit(
                &free_ranges,
                req.size,
                req.alignment,
                region_start,
                region_end,
            ) {
                Some(alloc_start) => {
                    let alloc_range = MemoryRange::new(alloc_start..alloc_start + req.size);
                    allocations[idx] = alloc_range;
                    free_ranges =
                        subtract_ranges(free_ranges.iter().copied(), [alloc_range]).collect();
                }
                None => {
                    let free_in_region: u64 = free_ranges
                        .iter()
                        .filter_map(|r| {
                            let eff_start = r.start().max(region_start);
                            let eff_end = r.end().min(region_end);
                            if eff_start < eff_end {
                                Some(eff_end - eff_start)
                            } else {
                                None
                            }
                        })
                        .sum();
                    return Err(AllocateError::Exhausted {
                        tag: req.tag.clone(),
                        size: req.size,
                        alignment: req.alignment,
                        constraint: req.constraint,
                        free_space: free_in_region,
                    });
                }
            }
        }

        // Step 4: Write results to targets and build sorted output.
        for (target, alloc) in self.targets.iter_mut().zip(allocations.iter()) {
            **target = *alloc;
        }

        allocations.sort();
        Ok(allocations)
    }
}

/// Finds the highest aligned start address within `[region_start, region_end)`
/// that fits `size` bytes within one of the free ranges.
///
/// The free list must be sorted by address. Iterates in reverse to find
/// the highest-address match first.
fn find_highest_fit(
    free_ranges: &[MemoryRange],
    size: u64,
    alignment: u64,
    region_start: u64,
    region_end: u64,
) -> Option<u64> {
    for range in free_ranges.iter().rev() {
        // Clip the free range to the constrained region.
        let eff_start = range.start().max(region_start);
        let eff_end = range.end().min(region_end);

        if eff_start >= eff_end || eff_end - eff_start < size {
            continue;
        }

        // Find the highest aligned start where [start, start + size) fits.
        let latest_start = eff_end - size;
        let aligned_start = latest_start & !(alignment - 1);

        if aligned_start >= eff_start {
            return Some(aligned_start);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn empty_input() {
        let builder = LayoutBuilder::new(48);
        let sorted = builder.allocate().unwrap();
        assert!(sorted.is_empty());
    }

    #[test]
    fn single_pinned() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request(
            "test",
            &mut target,
            4 * MIB,
            PAGE_SIZE,
            Constraint::Pinned(0xFC00_0000),
        );
        let sorted = builder.allocate().unwrap();
        assert_eq!(target, MemoryRange::new(0xFC00_0000..0xFC00_0000 + 4 * MIB));
        assert_eq!(sorted.len(), 1);
    }

    #[test]
    fn multiple_pinned_non_overlapping() {
        let mut t1 = MemoryRange::EMPTY;
        let mut t2 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("a", &mut t1, 4 * KIB, PAGE_SIZE, Constraint::Pinned(0x1000));
        builder.request("b", &mut t2, 4 * KIB, PAGE_SIZE, Constraint::Pinned(0x2000));
        let sorted = builder.allocate().unwrap();
        assert_eq!(t1, MemoryRange::new(0x1000..0x2000));
        assert_eq!(t2, MemoryRange::new(0x2000..0x3000));
        assert_eq!(sorted[0], MemoryRange::new(0x1000..0x2000));
        assert_eq!(sorted[1], MemoryRange::new(0x2000..0x3000));
    }

    #[test]
    fn pinned_overlap_rejected() {
        let mut t1 = MemoryRange::EMPTY;
        let mut t2 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("a", &mut t1, 8 * KIB, PAGE_SIZE, Constraint::Pinned(0x1000));
        builder.request("b", &mut t2, 4 * KIB, PAGE_SIZE, Constraint::Pinned(0x2000));
        let err = builder.allocate().unwrap_err();
        assert!(matches!(err, AllocateError::PinnedOverlap { .. }));
    }

    #[test]
    fn pinned_out_of_bounds() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(32);
        builder.request(
            "oob",
            &mut target,
            8 * KIB,
            PAGE_SIZE,
            Constraint::Pinned(0xFFFF_F000),
        );
        let err = builder.allocate().unwrap_err();
        assert!(matches!(err, AllocateError::PinnedOutOfBounds { .. }));
    }

    #[test]
    fn pinned_at_edge_of_address_space() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(32);
        builder.request(
            "edge",
            &mut target,
            4 * KIB,
            PAGE_SIZE,
            Constraint::Pinned(0xFFFF_F000),
        );
        builder.allocate().unwrap();
        assert_eq!(target, MemoryRange::new(0xFFFF_F000..0x1_0000_0000));
    }

    #[test]
    fn pinned_at_address_zero() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request(
            "zero",
            &mut target,
            4 * KIB,
            PAGE_SIZE,
            Constraint::Pinned(0),
        );
        builder.allocate().unwrap();
        assert_eq!(target, MemoryRange::new(0..PAGE_SIZE));
    }

    #[test]
    fn single_below_4gib() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("test", &mut target, MIB, MIB, Constraint::Below4GiB);
        builder.allocate().unwrap();
        assert!(target.end() <= FOUR_GIB);
        assert_eq!(target.len(), MIB);
        assert_eq!(target.start() % MIB, 0);
    }

    #[test]
    fn single_above_4gib() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("test", &mut target, GIB, MIB, Constraint::Above4GiB);
        builder.allocate().unwrap();
        assert!(target.start() >= FOUR_GIB);
        assert_eq!(target.len(), GIB);
        assert_eq!(target.start() % MIB, 0);
    }

    #[test]
    fn below_4gib_top_down_placement() {
        let mut t1 = MemoryRange::EMPTY;
        let mut t2 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("a", &mut t1, MIB, MIB, Constraint::Below4GiB);
        builder.request("b", &mut t2, MIB, MIB, Constraint::Below4GiB);
        builder.allocate().unwrap();
        // Same alignment and size → input order tiebreaker. t1 (order 0)
        // is placed first (highest address), t2 (order 1) gets the next
        // highest.
        assert!(t1.start() > t2.start());
        assert!(!t1.overlaps(&t2));
    }

    #[test]
    fn alignment_driven_ordering() {
        let mut small = MemoryRange::EMPTY;
        let mut big = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("small", &mut small, MIB, MIB, Constraint::Below4GiB);
        builder.request("big", &mut big, MIB, 256 * MIB, Constraint::Below4GiB);
        builder.allocate().unwrap();
        assert_eq!(big.start() % (256 * MIB), 0);
        assert_eq!(small.start() % MIB, 0);
        assert!(!big.overlaps(&small));
        assert!(big.end() <= FOUR_GIB);
        assert!(small.end() <= FOUR_GIB);
    }

    #[test]
    fn size_driven_ordering_same_alignment() {
        let mut small = MemoryRange::EMPTY;
        let mut big = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("small", &mut small, MIB, MIB, Constraint::Below4GiB);
        builder.request("big", &mut big, 4 * MIB, MIB, Constraint::Below4GiB);
        builder.allocate().unwrap();
        assert!(big.start() > small.start());
    }

    #[test]
    fn pinned_plus_dynamic() {
        let mut reserved = MemoryRange::EMPTY;
        let mut dynamic = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(32);
        builder.request(
            "reserved",
            &mut reserved,
            32 * MIB,
            PAGE_SIZE,
            Constraint::Pinned(0xFE00_0000),
        );
        builder.request("dynamic", &mut dynamic, MIB, MIB, Constraint::Below4GiB);
        builder.allocate().unwrap();
        assert_eq!(reserved, MemoryRange::new(0xFE00_0000..0x1_0000_0000));
        assert!(!dynamic.overlaps(&reserved));
        assert!(dynamic.end() <= FOUR_GIB);
    }

    #[test]
    fn exhaustion_below_4gib() {
        let mut t1 = MemoryRange::EMPTY;
        let mut t2 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(32);
        builder.request("pin", &mut t1, GIB, PAGE_SIZE, Constraint::Pinned(0));
        builder.request(
            "too_big",
            &mut t2,
            4 * GIB,
            PAGE_SIZE,
            Constraint::Below4GiB,
        );
        let err = builder.allocate().unwrap_err();
        assert!(matches!(err, AllocateError::Exhausted { .. }));
    }

    #[test]
    fn exhaustion_above_4gib_narrow_width() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(32);
        builder.request(
            "above",
            &mut target,
            PAGE_SIZE,
            PAGE_SIZE,
            Constraint::Above4GiB,
        );
        let err = builder.allocate().unwrap_err();
        assert!(matches!(err, AllocateError::Exhausted { .. }));
    }

    #[test]
    fn exhaustion_alignment_fragmentation() {
        let mut t1 = MemoryRange::EMPTY;
        let mut t2 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(36);
        builder.request(
            "pin",
            &mut t1,
            0xF800_0000,
            PAGE_SIZE,
            Constraint::Pinned(0),
        );
        builder.request(
            "misaligned",
            &mut t2,
            128 * MIB,
            256 * MIB,
            Constraint::Below4GiB,
        );
        let err = builder.allocate().unwrap_err();
        assert!(matches!(err, AllocateError::Exhausted { .. }));
    }

    #[test]
    fn below_4gib_above_4gib_filtering() {
        let mut below = MemoryRange::EMPTY;
        let mut above = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("below", &mut below, MIB, MIB, Constraint::Below4GiB);
        builder.request("above", &mut above, MIB, MIB, Constraint::Above4GiB);
        builder.allocate().unwrap();
        assert!(below.end() <= FOUR_GIB);
        assert!(above.start() >= FOUR_GIB);
    }

    #[test]
    fn sorted_ranges_order() {
        let mut t_above = MemoryRange::EMPTY;
        let mut t_pinned = MemoryRange::EMPTY;
        let mut t_below = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("above", &mut t_above, GIB, MIB, Constraint::Above4GiB);
        builder.request(
            "pinned",
            &mut t_pinned,
            PAGE_SIZE,
            PAGE_SIZE,
            Constraint::Pinned(0x1000),
        );
        builder.request("below", &mut t_below, MIB, MIB, Constraint::Below4GiB);
        let sorted = builder.allocate().unwrap();
        assert_eq!(sorted.len(), 3);
        // Pinned at 0x1000 should be first.
        assert_eq!(sorted[0], t_pinned);
        for [a, b] in sorted.array_windows() {
            assert!(a.start() < b.start());
        }
    }

    #[test]
    fn determinism() {
        let mut prev_sorted: Option<Vec<MemoryRange>> = None;
        for _ in 0..10 {
            let mut a = MemoryRange::EMPTY;
            let mut b = MemoryRange::EMPTY;
            let mut c = MemoryRange::EMPTY;
            let mut d = MemoryRange::EMPTY;
            let mut e = MemoryRange::EMPTY;
            let mut builder = LayoutBuilder::new(48);
            builder.request("a", &mut a, 4 * MIB, MIB, Constraint::Below4GiB);
            builder.request("b", &mut b, GIB, 256 * MIB, Constraint::Above4GiB);
            builder.request(
                "c",
                &mut c,
                32 * MIB,
                PAGE_SIZE,
                Constraint::Pinned(0xFE00_0000),
            );
            builder.request("d", &mut d, 128 * MIB, MIB, Constraint::Below4GiB);
            builder.request("e", &mut e, PAGE_SIZE, PAGE_SIZE, Constraint::Below4GiB);
            let sorted = builder.allocate().unwrap();
            if let Some(prev) = &prev_sorted {
                assert_eq!(prev, &sorted);
            }
            prev_sorted = Some(sorted);
        }
    }

    #[test]
    fn invalid_address_width() {
        let builder = LayoutBuilder::new(0);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidAddressWidth(0)
        ));
        let builder = LayoutBuilder::new(64);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidAddressWidth(64)
        ));
    }

    #[test]
    fn invalid_size() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("bad", &mut target, 0, PAGE_SIZE, Constraint::Below4GiB);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidSize { .. }
        ));

        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("bad", &mut target, 100, PAGE_SIZE, Constraint::Below4GiB);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidSize { .. }
        ));
    }

    #[test]
    fn invalid_alignment() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("bad", &mut target, PAGE_SIZE, 1024, Constraint::Below4GiB);
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidAlignment { .. }
        ));

        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request(
            "bad",
            &mut target,
            PAGE_SIZE,
            6 * KIB,
            Constraint::Below4GiB,
        );
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidAlignment { .. }
        ));
    }

    #[test]
    fn invalid_pinned_address() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request(
            "bad",
            &mut target,
            PAGE_SIZE,
            PAGE_SIZE,
            Constraint::Pinned(0x1234),
        );
        assert!(matches!(
            builder.allocate().unwrap_err(),
            AllocateError::InvalidPinnedAddress { .. }
        ));
    }

    #[test]
    fn realistic_x86_layout() {
        let mut reserved = MemoryRange::EMPTY;
        let mut vmbus_low = MemoryRange::EMPTY;
        let mut vmbus_high = MemoryRange::EMPTY;
        let mut pcie_ecam = MemoryRange::EMPTY;
        let mut pcie_low = MemoryRange::EMPTY;
        let mut pcie_high = MemoryRange::EMPTY;
        let mut virtio = [MemoryRange::EMPTY; 4];

        let mut builder = LayoutBuilder::new(48);
        builder.request(
            "reserved",
            &mut reserved,
            32 * MIB,
            PAGE_SIZE,
            Constraint::Pinned(0xFE00_0000),
        );
        builder.request(
            "vmbus_low",
            &mut vmbus_low,
            128 * MIB,
            MIB,
            Constraint::Below4GiB,
        );
        builder.request(
            "vmbus_high",
            &mut vmbus_high,
            GIB,
            MIB,
            Constraint::Above4GiB,
        );
        builder.request(
            "pcie_ecam",
            &mut pcie_ecam,
            256 * MIB,
            256 * MIB,
            Constraint::Below4GiB,
        );
        builder.request(
            "pcie_low",
            &mut pcie_low,
            64 * MIB,
            MIB,
            Constraint::Below4GiB,
        );
        builder.request("pcie_high", &mut pcie_high, GIB, MIB, Constraint::Above4GiB);
        for (i, v) in virtio.iter_mut().enumerate() {
            builder.request(
                format!("virtio_{i}"),
                v,
                PAGE_SIZE,
                PAGE_SIZE,
                Constraint::Below4GiB,
            );
        }

        let sorted = builder.allocate().unwrap();

        assert_eq!(reserved, MemoryRange::new(0xFE00_0000..0x1_0000_0000));
        assert!(vmbus_low.end() <= FOUR_GIB);
        assert!(pcie_ecam.end() <= FOUR_GIB);
        assert!(pcie_low.end() <= FOUR_GIB);
        for v in &virtio {
            assert!(v.end() <= FOUR_GIB);
        }
        assert!(vmbus_high.start() >= FOUR_GIB);
        assert!(pcie_high.start() >= FOUR_GIB);

        for [a, b] in sorted.array_windows() {
            assert!(a.end() <= b.start(), "overlap: {} and {}", a, b);
        }

        assert_eq!(pcie_ecam.start() % (256 * MIB), 0);

        for r in &sorted {
            assert!(r.end() <= 1u64 << 48);
        }
    }

    #[test]
    fn realistic_aarch64_layout() {
        let mut reserved = MemoryRange::EMPTY;
        let mut vmbus_low = MemoryRange::EMPTY;
        let mut vmbus_high = MemoryRange::EMPTY;

        let mut builder = LayoutBuilder::new(48);
        builder.request(
            "reserved",
            &mut reserved,
            272 * MIB,
            PAGE_SIZE,
            Constraint::Pinned(0xEF00_0000),
        );
        builder.request(
            "vmbus_low",
            &mut vmbus_low,
            128 * MIB,
            MIB,
            Constraint::Below4GiB,
        );
        builder.request(
            "vmbus_high",
            &mut vmbus_high,
            GIB,
            MIB,
            Constraint::Above4GiB,
        );

        let sorted = builder.allocate().unwrap();

        assert_eq!(reserved, MemoryRange::new(0xEF00_0000..0x1_0000_0000));
        assert!(vmbus_low.end() <= FOUR_GIB);
        assert!(vmbus_high.start() >= FOUR_GIB);

        for [a, b] in sorted.array_windows() {
            assert!(a.end() <= b.start());
        }
    }

    #[test]
    fn pinned_at_top_of_space() {
        let mut target = MemoryRange::EMPTY;
        let width: u8 = 36;
        let limit = 1u64 << width;
        let mut builder = LayoutBuilder::new(width);
        builder.request(
            "top",
            &mut target,
            PAGE_SIZE,
            PAGE_SIZE,
            Constraint::Pinned(limit - PAGE_SIZE),
        );
        builder.allocate().unwrap();
        assert_eq!(target, MemoryRange::new((limit - PAGE_SIZE)..limit));
    }

    #[test]
    fn many_small_allocations() {
        let mut targets = [MemoryRange::EMPTY; 100];
        let mut builder = LayoutBuilder::new(48);
        for (i, t) in targets.iter_mut().enumerate() {
            builder.request(
                format!("s{i}"),
                t,
                PAGE_SIZE,
                PAGE_SIZE,
                Constraint::Below4GiB,
            );
        }
        let sorted = builder.allocate().unwrap();
        assert_eq!(sorted.len(), 100);
        for [a, b] in sorted.array_windows() {
            assert!(a.end() <= b.start());
        }
        for r in &sorted {
            assert!(r.end() <= FOUR_GIB);
        }
    }

    #[test]
    fn mixed_constraints_with_pinned() {
        let mut p1 = MemoryRange::EMPTY;
        let mut p2 = MemoryRange::EMPTY;
        let mut d1 = MemoryRange::EMPTY;
        let mut d2 = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(48);
        builder.request("p1", &mut p1, GIB, PAGE_SIZE, Constraint::Pinned(GIB));
        builder.request("p2", &mut p2, GIB, PAGE_SIZE, Constraint::Pinned(3 * GIB));
        builder.request("d1", &mut d1, 512 * MIB, MIB, Constraint::Below4GiB);
        builder.request("d2", &mut d2, 512 * MIB, MIB, Constraint::Below4GiB);
        builder.allocate().unwrap();

        assert_eq!(p1, MemoryRange::new(GIB..2 * GIB));
        assert_eq!(p2, MemoryRange::new(3 * GIB..4 * GIB));

        assert!(!d1.overlaps(&d2));
        assert!(!d1.overlaps(&p1));
        assert!(!d1.overlaps(&p2));
        assert!(!d2.overlaps(&p1));
        assert!(!d2.overlaps(&p2));
    }

    #[test]
    fn narrow_address_space() {
        let mut target = MemoryRange::EMPTY;
        let mut builder = LayoutBuilder::new(20);
        builder.request(
            "test",
            &mut target,
            PAGE_SIZE,
            PAGE_SIZE,
            Constraint::Below4GiB,
        );
        builder.allocate().unwrap();
        assert!(target.end() <= 1u64 << 20);
    }
}
