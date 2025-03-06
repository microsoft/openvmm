// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Allocator for pages within a pool.
//!
//! This is used for temporary allocations of per-queue DMA buffers, mainly for
//! PRP lists.

use crate::memory::MemoryBlock;
use crate::memory::PAGE_SIZE;
use crate::memory::PAGE_SIZE64;
use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use guestmem::GuestMemoryError;
use inspect::Inspect;
use parking_lot::Mutex;
use std::sync::atomic::AtomicU8;

#[derive(Inspect)]
pub struct PageAllocator {
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
    /// Create a new page allocator. `mem` must be page-aligned in both size and
    /// offset. `max_allocation_size_pages` specifies the maximum allocation
    /// size in terms of number of pages, it must be less than or equal to
    /// `mem`'s size.
    pub fn new(mem: MemoryBlock, max_allocation_size_pages: usize) -> anyhow::Result<Self> {
        if mem.offset_in_page() != 0 || mem.len() % PAGE_SIZE != 0 {
            anyhow::bail!("memory must be page-aligned");
        }

        let page_count = mem.len() / PAGE_SIZE;
        if max_allocation_size_pages > page_count {
            anyhow::bail!("max allocation size must be less than or equal to memory size");
        }

        Ok(Self {
            core: Mutex::new(PageAllocatorCore::new(page_count)),
            mem,
            event: Default::default(),
            max: max_allocation_size_pages,
        })
    }

    /// Allocate `n` pages. This may block until enough pages are available.
    ///
    /// Returns `None` if the allocation is unable to succeed, due to being
    /// larger than the pool or the configured maximum allocation size.
    pub async fn alloc_pages(&self, n: usize) -> Option<ScopedPages<'_>> {
        if self.max < n {
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

    /// Allocate `n` bytes, which may block until enough bytes are available.
    ///
    /// Returns `None` if the allocation is unable to succeed, due to being
    /// larger than the pool or the configured maximum allocation size.
    pub async fn alloc_bytes(&self, n: usize) -> Option<ScopedPages<'_>> {
        self.alloc_pages(n.div_ceil(PAGE_SIZE)).await
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

impl std::fmt::Debug for ScopedPages<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedPages")
            .field("pages", &self.pages)
            .finish()
    }
}

#[derive(Debug)]
struct ScopedPage {
    page_index: usize,
    physical_address: u64,
}

impl ScopedPages<'_> {
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn physical_address(&self, index: usize) -> u64 {
        self.pages[index].physical_address
    }

    pub fn pfn(&self, index: usize) -> u64 {
        self.pages[index].physical_address / PAGE_SIZE64
    }

    pub fn pfns(&self) -> impl Iterator<Item = u64> + use<'_> {
        self.pages.iter().map(|p| p.physical_address / PAGE_SIZE64)
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

#[cfg(test)]
mod tests {
    use super::PageAllocator;
    use crate::emulated::DeviceSharedMemory;
    use crate::memory::MemoryBlock;
    use crate::memory::PAGE_SIZE;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use pal_async::DefaultDriver;
    use std::sync::Arc;

    #[async_test]
    async fn test_alloc(driver: DefaultDriver) {
        let size = PAGE_SIZE * 10;
        let mem = DeviceSharedMemory::new(size, 0);
        let block = MemoryBlock::new(mem.alloc(size).unwrap());
        let alloc = Arc::new(PageAllocator::new(block, 10).unwrap());
        let pages = alloc.alloc_pages(10).await.unwrap();
        assert_eq!(pages.page_count(), 10);
        let alloc2 = alloc.clone();
        let other_allocs = driver.spawn("test-allocs", async move {
            let _a = alloc2.alloc_pages(4).await;
            let _b = alloc2.alloc_pages(4).await;
            let _c = alloc2.alloc_pages(2).await;
        });
        drop(pages);
        other_allocs.await;
    }

    #[async_test]
    async fn test_alloc_size() {
        let pages = 10;
        let size = PAGE_SIZE * pages;
        let mem = DeviceSharedMemory::new(size, 0);
        let block = MemoryBlock::new(mem.alloc(size).unwrap());
        let alloc = PageAllocator::new(block, pages - 1).unwrap();

        let buf = alloc.alloc_pages(pages).await;
        assert!(buf.is_none());
        let buf = alloc.alloc_pages(pages + 10).await;
        assert!(buf.is_none());
        let buf = alloc.alloc_pages(pages - 1).await;
        assert!(buf.is_some());
        drop(buf);
        let buf = alloc.alloc_bytes(size).await;
        assert!(buf.is_none());
        let buf = alloc.alloc_bytes(size - 1).await;
        assert!(buf.is_none());
        let buf = alloc.alloc_bytes(size - PAGE_SIZE).await;
        assert!(buf.is_some());
        drop(buf);
        let buf = alloc.alloc_bytes(size * 2).await;
        assert!(buf.is_none());
    }
}
