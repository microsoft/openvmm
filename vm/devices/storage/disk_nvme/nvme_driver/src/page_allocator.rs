// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Allocator for pages within a pool.
//!
//! This is used for temporary allocations of per-queue DMA buffers, mainly for
//! PRP lists.

use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::atomic::AtomicU8;
use user_driver::memory::MemoryBlock;
use user_driver::memory::PAGE_SIZE;
use user_driver::memory::PAGE_SIZE64;

#[derive(Inspect)]
pub(crate) struct PageAllocator {
    #[inspect(flatten)]
    core: Mutex<PageAllocatorCore>,
    #[inspect(skip)]
    mem: MemoryBlock,
    #[inspect(skip)]
    event: event_listener::Event,
    max: usize,
}

impl std::fmt::Debug for PageAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageAllocator").finish()
    }
}

impl PageAllocator {
    pub fn new(mem: MemoryBlock) -> Self {
        assert_eq!(mem.offset_in_page(), 0);
        assert_eq!(mem.len() % PAGE_SIZE, 0);
        let count = mem.len() / PAGE_SIZE;
        Self {
            core: Mutex::new(PageAllocatorCore::new(count)),
            mem,
            event: Default::default(),
            max: count,
        }
    }

    pub async fn alloc_pages(&self, n: usize) -> Option<ScopedPages<'_>> {
        // A single page must be left over for the PRP list, so one request may
        // not use all pages.
        if self.max < n + 1 {
            return None;
        }
        let mut core = loop {
            let listener = {
                let core = self.core.lock();
                if core.remaining() >= n {
                    break core;
                }
                // Fairness is pretty bad with this approach--small allocations
                // could easily prevent a large allocation from ever succeeding.
                // But we don't really have this use case right now, so this is OK.
                self.event.listen()
            };
            listener.await;
        };

        let pfns = self.mem.pfns();
        let pages = (0..n)
            .map(|_| {
                let n = core.alloc().unwrap();
                ScopedPage {
                    page_index: n,
                    physical_address: pfns[n] * PAGE_SIZE64,
                }
            })
            .collect();
        Some(ScopedPages { alloc: self, pages })
    }

    pub async fn alloc_bytes(&self, n: usize) -> Option<ScopedPages<'_>> {
        self.alloc_pages((n + PAGE_SIZE - 1) / PAGE_SIZE).await
    }
}

#[derive(Inspect)]
struct PageAllocatorCore {
    #[inspect(with = "|x| x.len()")]
    free: Vec<usize>,
}

impl PageAllocatorCore {
    fn new(count: usize) -> Self {
        let free = (0..count).rev().collect();
        Self { free }
    }

    fn remaining(&self) -> usize {
        self.free.len()
    }

    fn alloc(&mut self) -> Option<usize> {
        self.free.pop()
    }

    fn free(&mut self, n: usize) {
        self.free.push(n);
    }
}

pub struct ScopedPages<'a> {
    alloc: &'a PageAllocator,
    pages: Vec<ScopedPage>,
}

struct ScopedPage {
    page_index: usize,
    physical_address: u64,
}

impl<'a> ScopedPages<'a> {
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn physical_address(&self, index: usize) -> u64 {
        self.pages[index].physical_address
    }

    pub fn page_as_slice(&self, index: usize) -> &[AtomicU8] {
        &self.alloc.mem.as_slice()[self.pages[index].page_index * PAGE_SIZE..][..PAGE_SIZE]
    }

    pub fn read(&self, data: &mut [u8]) {
        assert!(data.len() <= self.pages.len() * PAGE_SIZE);
        for (chunk, page) in data.chunks_mut(PAGE_SIZE).zip(&self.pages) {
            self.alloc.mem.read_at(page.page_index * PAGE_SIZE, chunk);
        }
    }

    pub fn copy_to_guest_memory(
        &self,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<(), GuestMemoryError> {
        let mut remaining = mem.len();
        for (i, page) in self.pages.iter().enumerate() {
            let len = PAGE_SIZE.min(remaining);
            remaining -= len;
            guest_memory.write_range_from_atomic(
                &mem.subrange(i * PAGE_SIZE, len),
                &self.alloc.mem.as_slice()[page.page_index * PAGE_SIZE..][..len],
            )?;
        }
        Ok(())
    }

    pub fn write(&self, data: &[u8]) {
        assert!(data.len() <= self.pages.len() * PAGE_SIZE);
        for (chunk, page) in data.chunks(PAGE_SIZE).zip(&self.pages) {
            self.alloc.mem.write_at(page.page_index * PAGE_SIZE, chunk);
        }
    }

    pub fn copy_from_guest_memory(
        &self,
        guest_memory: &GuestMemory,
        mem: PagedRange<'_>,
    ) -> Result<(), GuestMemoryError> {
        let mut remaining = mem.len();
        for (i, page) in self.pages.iter().enumerate() {
            let len = PAGE_SIZE.min(remaining);
            remaining -= len;
            guest_memory.read_range_to_atomic(
                &mem.subrange(i * PAGE_SIZE, len),
                &self.alloc.mem.as_slice()[page.page_index * PAGE_SIZE..][..len],
            )?;
        }
        Ok(())
    }
}

impl Drop for ScopedPages<'_> {
    fn drop(&mut self) {
        let n = self.pages.len();
        {
            let mut core = self.alloc.core.lock();
            for page in self.pages.drain(..) {
                core.free(page.page_index);
            }
        }
        self.alloc.event.notify_additional(n);
    }
}
