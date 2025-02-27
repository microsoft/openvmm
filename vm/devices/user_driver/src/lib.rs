// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Infrastructure for implementing PCI drivers in user mode.

// UNSAFETY: Manual memory management around buffers and mmap.
#![expect(unsafe_code)]

use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use inspect::Inspect;
use interrupt::DeviceInterrupt;
use memory::MemoryBlock;
use page_allocator::ScopedPages;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub mod backoff;
pub mod emulated;
pub mod interrupt;
pub mod lockmem;
pub mod memory;
pub mod page_allocator;
pub mod vfio;

/// An interface to access device hardware.
pub trait DeviceBacking: 'static + Send + Inspect {
    /// An object for accessing device registers.
    type Registers: 'static + DeviceRegisterIo + Inspect;

    /// Returns a device ID for diagnostics.
    fn id(&self) -> &str;

    /// Maps a BAR.
    fn map_bar(&mut self, n: u8) -> anyhow::Result<Self::Registers>;

    /// DMA Client for the device.
    fn dma_client(&self) -> Arc<dyn DmaClient>;

    /// Returns the maximum number of interrupts that can be mapped.
    fn max_interrupt_count(&self) -> u32;

    /// Maps a MSI-X interrupt for use, returning an object that can be used to
    /// wait for the interrupt to be signaled by the device.
    ///
    /// `cpu` is the CPU that the device should target with this interrupt.
    ///
    /// This can be called multiple times for the same interrupt without disconnecting
    /// previous mappings. The last `cpu` value will be used as the target CPU.
    fn map_interrupt(&mut self, msix: u32, cpu: u32) -> anyhow::Result<DeviceInterrupt>;
}

/// Access to device registers.
pub trait DeviceRegisterIo: Send + Sync {
    /// Returns the length of the register space.
    fn len(&self) -> usize;
    /// Reads a `u32` register.
    fn read_u32(&self, offset: usize) -> u32;
    /// Reads a `u64` register.
    fn read_u64(&self, offset: usize) -> u64;
    /// Writes a `u32` register.
    fn write_u32(&self, offset: usize, data: u32);
    /// Writes a `u64` register.
    fn write_u64(&self, offset: usize, data: u64);
}

#[derive(Debug, thiserror::Error)]
pub enum MapDmaError {
    #[error("failed to map ranges")]
    MapFailed,
    #[error("no bounce buffers available")]
    NoBounceBufferAvailable,
    // UnmapFailed,
    // PinFailed,
    // BounceBufferFailed,
}

#[derive(Debug)]
pub struct MapDmaOptions {
    pub always_bounce: bool,
    pub is_rx: bool,
    pub is_tx: bool,
    // todo?
    // pub non_blocking: bool,
}

/// what we did to pages in a transaction
#[derive(Debug)]
pub enum DmaOperation<'a> {
    /// pages were already pinned/physically backed, original ranges
    PrePinned(PagedRange<'a>),
    /// pinned pages, must be unpinned, original ranges
    Pinned(PagedRange<'a>),
    /// allocated bounce buffers, original ranges
    Bounced(ScopedPages<'a>, PagedRange<'a>),
}

enum DmaOperationIter<A, B> {
    PagedRange(A),
    ScopedPages(B),
}

impl<A: Iterator, B: Iterator<Item = A::Item>> Iterator for DmaOperationIter<A, B> {
    type Item = A::Item;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            DmaOperationIter::PagedRange(iter) => iter.next(),
            DmaOperationIter::ScopedPages(iter) => iter.next(),
        }
    }
}

impl DmaOperation<'_> {
    /// the mapped ranges for this transaction
    pub fn gpns_iter(&self) -> impl Iterator<Item = u64> + '_ {
        match self {
            DmaOperation::PrePinned(r) => DmaOperationIter::PagedRange(r.gpns().iter().copied()),
            DmaOperation::Pinned(r) => DmaOperationIter::PagedRange(r.gpns().iter().copied()),
            DmaOperation::Bounced(pages, _) => DmaOperationIter::ScopedPages(pages.pfns()),
        }
    }
}

// TODO: make trait w/ associated type in dmaclient return for map to allow dma client implementer to hide details
#[derive(Debug)]
pub struct DmaTransaction<'a> {
    /// guest memory object to use to bounce in/out
    pub guest_memory: &'a GuestMemory,
    pub operation: DmaOperation<'a>,
    pub options: MapDmaOptions,
}

impl DmaTransaction<'_> {
    /// the mapped ranges for this transaction
    pub fn pfns(&self) -> impl Iterator<Item = u64> + '_ {
        self.operation.gpns_iter()
    }
}

/// Device interfaces for DMA.
pub trait DmaClient: Send + Sync + Inspect {
    /// Allocate a new DMA buffer. This buffer must be zero initialized.
    ///
    /// TODO: string tag for allocation?
    fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock>;

    /// Attach to a previously allocated memory block.
    fn attach_dma_buffer(&self, len: usize, base_pfn: u64) -> anyhow::Result<MemoryBlock>;

    // Do these methods move? Do we have some other trait that just provides
    // pin/unpin/query pin required, then this code moves up into common code
    // and is not a trait method?
    //
    // This would remove the Box<..> for the async closure.

    /// Map the given ranges for DMA. A caller must call `unmap_dma_ranges` to
    /// complete a dma transaction to observe the dma in the passed in ranges.
    ///
    /// This function may block, as if a page is required to be bounced it may
    /// block waiting for bounce buffer space to become available.
    fn map_dma_ranges<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Pin<Box<dyn Future<Output = Result<DmaTransaction<'a>, MapDmaError>> + 'a>>;

    /// Unmap a dma transaction to observe the dma into the requested ranges.
    ///
    /// TODO: return original ranges?
    fn unmap_dma_ranges(&self, transaction: DmaTransaction<'_>) -> Result<(), MapDmaError>;
}
