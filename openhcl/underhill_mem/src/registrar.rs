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
//! This is done by registering memory in 2GB chunks, which is large enough to
//! get large pages in the kernel, but small enough to keep the overhead of the
//! initial registration for a chunk small. We track whether a given chunk has
//! been registered via a small bitmap.

use cvm_tracing::CVM_ALLOWED;
use inspect::Inspect;
use memory_range::MemoryRange;
use memory_range::overlapping_ranges;
use parking_lot::Mutex;
use std::ops::Range;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Acquire;
use std::sync::atomic::Ordering::Release;
use vm_topology::memory::MemoryLayout;

const PAGE_SIZE: u64 = guestmem::PAGE_SIZE as u64;

#[derive(Debug)]
pub struct MemoryRegistrar<T> {
    registered: Bitmap,
    chunk_count: u64,
    state: Mutex<RegistrarState>,
    register: T,
    ram: Vec<MemoryRange>,
    registration_offset: u64,
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
            .hex("registration_offset", self.registration_offset);
    }
}

#[derive(Debug)]
struct RegistrarState {
    failed: Bitmap,
}

#[derive(Debug)]
struct Bitmap(Vec<AtomicU64>);

impl Bitmap {
    fn new(address_space_size: u64) -> Self {
        let chunks = address_space_size.div_ceil(GRANULARITY);
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

/// Register in 2GB chunks.
const GRANULARITY: u64 = 2 << 30;

impl<T: RegisterMemory> MemoryRegistrar<T> {
    pub fn new(layout: &MemoryLayout, registration_offset: u64, register: T) -> Self {
        let address_space_size = layout.ram().last().unwrap().range.end();

        Self {
            chunk_count: address_space_size.div_ceil(GRANULARITY),
            registered: Bitmap::new(address_space_size),
            state: Mutex::new(RegistrarState {
                failed: Bitmap::new(address_space_size),
            }),
            register,
            ram: layout.ram().iter().map(|r| r.range).collect(),
            registration_offset,
        }
    }

    fn chunks(range: MemoryRange) -> Range<u64> {
        let start = range.start() / GRANULARITY;
        let end = range.end().div_ceil(GRANULARITY);
        start..end
    }

    pub fn register(&self, address: u64, len: u64) -> Result<(), u64> {
        // Page align the requested range.
        let requested_range = MemoryRange::new(
            address & !(PAGE_SIZE - 1)..(address + len + (PAGE_SIZE - 1)) & !(PAGE_SIZE - 1),
        );

        // Check if the range is already registered.
        'check_registered: {
            for chunk in Self::chunks(requested_range) {
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
        for chunk in Self::chunks(requested_range) {
            if self.registered.get(chunk) {
                continue;
            }
            if state.failed.get_mut(chunk) {
                return Err(chunk * GRANULARITY);
            }
            // Register the full chunk, bounded by the RAM regions. This could
            // be more efficient, but again, we expect there to only be one
            // chunk in practice.
            let full_range = MemoryRange::new(chunk * GRANULARITY..(chunk + 1) * GRANULARITY);
            for range in overlapping_ranges([full_range], self.ram.iter().copied()) {
                let range = MemoryRange::new(
                    self.registration_offset + range.start()
                        ..self.registration_offset + range.end(),
                );
                tracing::info!(CVM_ALLOWED, %range, "registering memory");
                if let Err(err) = self.register.register_range(range) {
                    tracing::error!(CVM_ALLOWED,
                        %range,
                        registration_offset = self.registration_offset,
                        error = &err as &dyn std::error::Error,
                        "failed to register memory"
                    );
                    state.failed.set_mut(chunk, true);
                    return Err(range.start());
                }
            }
            self.registered.set(chunk, true);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryRegistrar;
    use crate::registrar::GRANULARITY;
    use memory_range::MemoryRange;
    use std::cell::RefCell;
    use std::convert::Infallible;
    use vm_topology::memory::MemoryLayout;

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
        let registrar = MemoryRegistrar::new(&layout, offset, |range| {
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
}
