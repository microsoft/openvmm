// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Code to register lower VTL memory with the kernel as needed.
//!
//! For many kernel operations that operate on memory, such as passing a buffer
//! to a device for DMA, the kernel requires that has allocated a `struct page`
//! object for each page being accessed. Thanks to some optimizations for large
//! memory allocations, the space overhead of this for guest memory is not too
//! large, but the initialization time overhead can be significant for large
//! VMs.
//!
//! To avoid this overhead, we only register memory with the kernel as needed,
//! when a VA might leak out of a `GuestMemory` object and possibly be passed to
//! a kernel routine.
//!
//! Memory is registered in caller-selected chunks. We track whether a given
//! chunk has been registered via a small bitmap.

use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use memory_range::MemoryRange;
use memory_range::overlapping_ranges;
use parking_lot::Mutex;
use std::ops::Range;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Release;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;
use vm_topology::memory::MemoryRangeWithNode;

const PAGE_SIZE: u64 = guestmem::PAGE_SIZE as u64;

#[derive(Debug)]
pub struct MemoryRegistrar<T> {
    registered: Bitmap,
    chunk_count: u64,
    state: Mutex<RegistrarState>,
    register: T,
    ram: Vec<MemoryRange>,
    ram_with_node: Vec<MemoryRangeWithNode>,
    registration_offset: u64,
    granularity: u64,
}

impl<T> Inspect for MemoryRegistrar<T> {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field_with("chunks_registered", || {
                (0..self.chunk_count)
                    .filter(|&chunk| self.registered.get(chunk))
                    .count()
            })
            .field("chunk_count", self.chunk_count)
            .field("granularity", self.granularity)
            .hex("registration_offset", self.registration_offset);
    }
}

#[derive(Debug)]
struct RegistrarState {
    failed: Bitmap,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum RegisterAllError {
    #[error("failed to register memory starting at {address:#x}")]
    RegistrationFailed { address: u64 },
    #[error(
        "VTL0 RAM span {span} has unregistrable edge {edge} in virtual NUMA node {vnode} range \
         {node_range}; kernel registration requires {alignment:#x}-aligned span boundaries"
    )]
    UnalignedMemory {
        span: MemoryRange,
        edge: MemoryRange,
        vnode: u32,
        node_range: MemoryRange,
        alignment: u64,
    },
}

#[derive(Debug)]
struct Bitmap(Vec<AtomicU64>);

impl Bitmap {
    fn new(address_space_size: u64, granularity: u64) -> Self {
        let chunks = address_space_size.div_ceil(granularity);
        let words = chunks.div_ceil(64);
        let mut v = Vec::new();
        v.resize_with(words as usize, AtomicU64::default);
        Self(v)
    }

    fn get(&self, chunk: u64) -> bool {
        self.0[chunk as usize / 64].load(Acquire) & (1 << (chunk % 64)) != 0
    }

    fn get_mut(&mut self, chunk: u64) -> bool {
        *self.0[chunk as usize / 64].get_mut() & (1 << (chunk % 64)) != 0
    }

    fn set(&self, chunk: u64, value: bool) {
        if value {
            self.0[chunk as usize / 64].fetch_or(1 << (chunk % 64), Release);
        } else {
            self.0[chunk as usize / 64].fetch_and(!(1 << (chunk % 64)), Release);
        }
    }

    fn set_mut(&mut self, chunk: u64, value: bool) {
        if value {
            *self.0[chunk as usize / 64].get_mut() |= 1 << (chunk % 64);
        } else {
            *self.0[chunk as usize / 64].get_mut() &= !(1 << (chunk % 64));
        }
    }
}

pub trait RegisterMemory {
    fn register_range(&self, range: MemoryRange) -> Result<(), impl 'static + std::error::Error>;
}

impl<T: Fn(MemoryRange) -> Result<(), E>, E: 'static + std::error::Error> RegisterMemory for T {
    fn register_range(&self, range: MemoryRange) -> Result<(), impl 'static + std::error::Error> {
        (self)(range)
    }
}

impl<T: RegisterMemory> MemoryRegistrar<T> {
    pub fn new(
        layout: &MemoryLayout,
        registration_offset: u64,
        granularity: u64,
        register: T,
    ) -> Self {
        assert!(granularity.is_power_of_two());
        assert!(granularity >= PAGE_SIZE);
        let address_space_size = layout.ram().last().unwrap().range.end();

        let mut ram: Vec<MemoryRange> = Vec::new();
        for range in layout.ram().iter().map(|entry| entry.range) {
            if let Some(previous) = ram.last_mut()
                && previous.end() == range.start()
            {
                *previous = MemoryRange::new(previous.start()..range.end());
            } else {
                ram.push(range);
            }
        }

        Self {
            chunk_count: address_space_size.div_ceil(granularity),
            registered: Bitmap::new(address_space_size, granularity),
            state: Mutex::new(RegistrarState {
                failed: Bitmap::new(address_space_size, granularity),
            }),
            register,
            ram,
            ram_with_node: layout.ram().to_vec(),
            registration_offset,
            granularity,
        }
    }

    fn chunks(&self, range: MemoryRange) -> Range<u64> {
        let start = range.start() / self.granularity;
        let end = range.end().div_ceil(self.granularity);
        start..end
    }

    fn register_range(&self, state: &mut RegistrarState, range: MemoryRange) -> Result<(), u64> {
        let registered_range = MemoryRange::new(
            self.registration_offset + range.start()..self.registration_offset + range.end(),
        );
        tracing::info!(CVM_ALLOWED, range = %registered_range, "registering memory");
        if let Err(err) = self.register.register_range(registered_range) {
            tracing::error!(
                CVM_ALLOWED,
                range = %registered_range,
                registration_offset = self.registration_offset,
                error = &err as &dyn std::error::Error,
                "failed to register memory"
            );
            for chunk in self.chunks(range) {
                state.failed.set_mut(chunk, true);
            }
            return Err(range.start());
        }
        Ok(())
    }

    pub fn register(&self, address: u64, len: u64) -> Result<(), u64> {
        // Page align the requested range.
        let requested_range = MemoryRange::new(
            address & !(PAGE_SIZE - 1)..(address + len + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1),
        );

        // Check if the range is already registered.
        'check_registered: {
            for chunk in self.chunks(requested_range) {
                if !self.registered.get(chunk) {
                    break 'check_registered;
                }
            }
            return Ok(());
        }

        // Register each chunk one at a time. We don't typically lock lots of
        // memory at a time, so in practice there should only be one chunk
        // anyway.
        let mut state = self.state.lock();
        for chunk in self.chunks(requested_range) {
            if self.registered.get(chunk) {
                continue;
            }
            if state.failed.get_mut(chunk) {
                return Err(chunk * self.granularity);
            }
            // Register the full chunk, bounded by the RAM regions. This could
            // be more efficient, but again, we expect there to only be one
            // chunk in practice.
            let full_range =
                MemoryRange::new(chunk * self.granularity..(chunk + 1) * self.granularity);
            for range in overlapping_ranges([full_range], self.ram.iter().copied()) {
                self.register_range(&mut state, range)?;
            }
            self.registered.set(chunk, true);
        }
        Ok(())
    }

    /// Register every complete `alignment`-aligned RAM subrange in the address space.
    ///
    /// If `ignore_unaligned_ranges` is false, fail if any RAM remains outside
    /// the aligned subranges.
    pub fn register_all_aligned(
        &self,
        alignment: u64,
        ignore_unaligned_ranges: bool,
    ) -> Result<(), RegisterAllError> {
        assert!(alignment.is_power_of_two());
        assert!(alignment >= self.granularity);

        let mut state = self.state.lock();
        for &span in &self.ram {
            let aligned_range = span.aligned_subrange(alignment);
            let unaligned_edge = if ignore_unaligned_ranges {
                None
            } else if aligned_range.is_empty() {
                Some(span)
            } else if span.start() != aligned_range.start() {
                Some(MemoryRange::new(span.start()..aligned_range.start()))
            } else if span.end() != aligned_range.end() {
                Some(MemoryRange::new(aligned_range.end()..span.end()))
            } else {
                None
            };
            if let Some(edge) = unaligned_edge {
                let entry = self
                    .ram_with_node
                    .iter()
                    .find(|entry| entry.range.overlaps(&edge))
                    .expect("edge belongs to a RAM range");
                return Err(RegisterAllError::UnalignedMemory {
                    span,
                    edge,
                    vnode: entry.vnode,
                    node_range: entry.range,
                    alignment,
                });
            }

            if aligned_range.is_empty() {
                continue;
            }

            let mut unregistered_run_start = None;

            for chunk in self.chunks(aligned_range) {
                if state.failed.get_mut(chunk) {
                    return Err(RegisterAllError::RegistrationFailed {
                        address: chunk * self.granularity,
                    });
                }
                if self.registered.get(chunk) {
                    if let Some(start) = unregistered_run_start.take() {
                        self.register_range(
                            &mut state,
                            MemoryRange::new(start..chunk * self.granularity),
                        )
                        .map_err(|address| RegisterAllError::RegistrationFailed { address })?;
                        for registered_chunk in start / self.granularity..chunk {
                            self.registered.set(registered_chunk, true);
                        }
                    }
                } else {
                    unregistered_run_start.get_or_insert(chunk * self.granularity);
                }
            }

            if let Some(start) = unregistered_run_start {
                let range = MemoryRange::new(start..aligned_range.end());
                self.register_range(&mut state, range)
                    .map_err(|address| RegisterAllError::RegistrationFailed { address })?;
                for chunk in self.chunks(range) {
                    self.registered.set(chunk, true);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryRegistrar;
    use super::RegisterAllError;
    use memory_range::MemoryRange;
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::convert::Infallible;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::memory::MemoryRangeWithNode;

    const GRANULARITY: u64 = 1 << 30;

    #[test]
    fn test_registrar() {
        let layout = MemoryLayout::new(
            1 << 40,
            &[
                MemoryRange::new(0x10000..0x20000),
                MemoryRange::new(1 << 40..2 << 40),
            ],
            &[],
            &[],
            None,
        )
        .unwrap();

        let offset = 1 << 50;
        let ranges = RefCell::new(Vec::new());
        let registrar = MemoryRegistrar::new(&layout, offset, GRANULARITY, |range| {
            println!("registering {:#x?}", range);
            ranges.borrow_mut().push(range);
            Ok::<_, Infallible>(())
        });

        for range in [
            0x1000..0x8000,
            0x20000..0x30000,
            0x100000..0x200000,
            1u64 << 33..(1u64 << 35) + 1,
        ] {
            registrar
                .register(range.start, range.end - range.start)
                .unwrap();
        }

        let mut expected = vec![
            MemoryRange::new(offset..offset | 0x10000),
            MemoryRange::new(offset | 0x20000..offset | GRANULARITY),
        ];
        expected.extend(
            (1 << 33..(1 << 35) + GRANULARITY)
                .step_by(GRANULARITY as usize)
                .map(|start| MemoryRange::new(offset | start..offset | (start + GRANULARITY))),
        );

        let ranges = ranges.take();
        assert_eq!(
            ranges.as_slice(),
            expected.as_slice(),
            "ranges: {}\n\nexpected: {}",
            ranges
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            expected
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    #[test]
    fn test_register_all_aligned_rejects_unaligned_edges_at_minimum_granularity() {
        let layout = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: MemoryRange::new(0x10000..2 * GRANULARITY + 0x20000),
                vnode: 7,
            }],
            &[],
        )
        .unwrap();

        let ranges = RefCell::new(Vec::new());
        let registrar = MemoryRegistrar::new(&layout, 0, GRANULARITY, |range| {
            ranges.borrow_mut().push(range);
            Ok::<_, Infallible>(())
        });

        assert_eq!(
            registrar.register_all_aligned(GRANULARITY, false),
            Err(RegisterAllError::UnalignedMemory {
                span: MemoryRange::new(0x10000..2 * GRANULARITY + 0x20000),
                edge: MemoryRange::new(0x10000..GRANULARITY),
                vnode: 7,
                node_range: MemoryRange::new(0x10000..2 * GRANULARITY + 0x20000),
                alignment: GRANULARITY,
            })
        );

        assert!(ranges.take().is_empty());
    }

    #[test]
    fn test_register_all_aligned_reports_repro_suffix_and_numa_node() {
        const PMD_GRANULARITY: u64 = 1 << 21;
        let span = MemoryRange::new(0x80000000..0xd8150000);
        let layout = MemoryLayout::new_from_ranges(
            &[
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x80000000..0xc0000000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(0xc0000000..0xd8150000),
                    vnode: 1,
                },
            ],
            &[],
        )
        .unwrap();

        let registrar =
            MemoryRegistrar::new(&layout, 0, PMD_GRANULARITY, |_| Ok::<_, Infallible>(()));

        assert_eq!(
            registrar.register_all_aligned(PMD_GRANULARITY, false),
            Err(RegisterAllError::UnalignedMemory {
                span,
                edge: MemoryRange::new(0xd8000000..0xd8150000),
                vnode: 1,
                node_range: MemoryRange::new(0xc0000000..0xd8150000),
                alignment: PMD_GRANULARITY,
            })
        );
    }

    #[test]
    fn test_register_all_aligned_rejects_span_smaller_than_granularity() {
        let span = MemoryRange::new(0x10000..0x20000);
        let layout = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: span,
                vnode: 2,
            }],
            &[],
        )
        .unwrap();

        let registrar = MemoryRegistrar::new(&layout, 0, GRANULARITY, |_| Ok::<_, Infallible>(()));

        assert_eq!(
            registrar.register_all_aligned(GRANULARITY, false),
            Err(RegisterAllError::UnalignedMemory {
                span,
                edge: span,
                vnode: 2,
                node_range: span,
                alignment: GRANULARITY,
            })
        );
    }

    #[test]
    fn test_register_all_aligned_uses_smaller_pages_for_edges() {
        const SMALL_GRANULARITY: u64 = 1 << 21;

        let layout = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: MemoryRange::new(SMALL_GRANULARITY..2 * GRANULARITY + SMALL_GRANULARITY),
                vnode: 0,
            }],
            &[],
        )
        .unwrap();

        let ranges = RefCell::new(Vec::new());
        let registrar = MemoryRegistrar::new(&layout, 0, SMALL_GRANULARITY, |range| {
            ranges.borrow_mut().push(range);
            Ok::<_, Infallible>(())
        });

        registrar.register_all_aligned(GRANULARITY, true).unwrap();
        registrar
            .register_all_aligned(SMALL_GRANULARITY, false)
            .unwrap();

        assert_eq!(registrar.register(SMALL_GRANULARITY, 0x1000), Ok(()));
        assert_eq!(registrar.register(2 * GRANULARITY, 0x1000), Ok(()));

        assert_eq!(
            ranges.take(),
            [
                MemoryRange::new(GRANULARITY..2 * GRANULARITY),
                MemoryRange::new(SMALL_GRANULARITY..GRANULARITY),
                MemoryRange::new(2 * GRANULARITY..2 * GRANULARITY + SMALL_GRANULARITY),
            ]
        );
    }

    #[test]
    fn test_register_all_aligned_multiple_ranges_with_gap() {
        const SMALL_GRANULARITY: u64 = 1 << 16;

        // A mix of RAM ranges with unbacked gaps between them: one spanning a
        // chunk boundary, one contained in a single chunk, one spanning
        // several whole chunks, and one crossing a boundary with unaligned ends.
        let layout = MemoryLayout::new_from_ranges(
            &[
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x10000..GRANULARITY + 0x20000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(3 * GRANULARITY + 0x10000..3 * GRANULARITY + 0x30000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(4 * GRANULARITY + 0x40000..4 * GRANULARITY + 0x50000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(5 * GRANULARITY..8 * GRANULARITY),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(9 * GRANULARITY + 0x30000..10 * GRANULARITY + 0x10000),
                    vnode: 0,
                },
            ],
            &[],
        )
        .unwrap();

        let ranges = RefCell::new(Vec::new());
        let registrar = MemoryRegistrar::new(&layout, 0, SMALL_GRANULARITY, |range| {
            ranges.borrow_mut().push(range);
            Ok::<_, Infallible>(())
        });

        registrar.register_all_aligned(GRANULARITY, true).unwrap();

        assert_eq!(
            ranges.take(),
            [MemoryRange::new(5 * GRANULARITY..8 * GRANULARITY)]
        );
    }

    #[test]
    fn test_register_all_aligned_coalesces_large_aligned_range() {
        const SMALL_GRANULARITY: u64 = 1 << 21;

        let layout = MemoryLayout::new_from_ranges(
            &[MemoryRangeWithNode {
                range: MemoryRange::new(0..3 * GRANULARITY),
                vnode: 0,
            }],
            &[],
        )
        .unwrap();

        let ranges = RefCell::new(Vec::new());
        let registrar = MemoryRegistrar::new(&layout, 0, SMALL_GRANULARITY, |range| {
            ranges.borrow_mut().push(range);
            Ok::<_, Infallible>(())
        });

        registrar.register_all_aligned(GRANULARITY, false).unwrap();

        assert_eq!(ranges.take(), [MemoryRange::new(0..3 * GRANULARITY)]);
    }

    #[test]
    fn test_register_all_aligned_merges_adjacent_numa_ranges() {
        let layout = MemoryLayout::new_from_ranges(
            &[
                MemoryRangeWithNode {
                    range: MemoryRange::new(0..GRANULARITY / 2),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(GRANULARITY / 2..GRANULARITY),
                    vnode: 1,
                },
            ],
            &[],
        )
        .unwrap();

        let ranges = RefCell::new(Vec::new());
        let registrar = MemoryRegistrar::new(&layout, 0, 1 << 21, |range| {
            ranges.borrow_mut().push(range);
            Ok::<_, Infallible>(())
        });

        registrar.register_all_aligned(GRANULARITY, false).unwrap();

        assert_eq!(ranges.take(), [MemoryRange::new(0..GRANULARITY)]);
    }

    #[test]
    fn test_failed_subrange_does_not_mark_chunk_registered() {
        let layout = MemoryLayout::new_from_ranges(
            &[
                MemoryRangeWithNode {
                    range: MemoryRange::new(0..0x10000),
                    vnode: 0,
                },
                MemoryRangeWithNode {
                    range: MemoryRange::new(0x20000..0x30000),
                    vnode: 0,
                },
            ],
            &[],
        )
        .unwrap();

        let calls = Cell::new(0);
        let registrar = MemoryRegistrar::new(&layout, 0, GRANULARITY, |_| {
            let call = calls.get();
            calls.set(call + 1);
            if call == 0 {
                Ok(())
            } else {
                Err(std::io::Error::other("registration failure"))
            }
        });

        assert!(registrar.register(0, 1).is_err());
        assert!(registrar.register(0, 1).is_err());
        assert_eq!(calls.get(), 2);
    }
}
