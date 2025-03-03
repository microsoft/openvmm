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
    fn dma_client(&self) -> &DmaClientDriver;

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
    Map,
    #[error("no bounce buffers available")]
    NoBounceBufferAvailable,
    // UnmapFailed,
    // PinFailed,
    // BounceBufferFailed,
    #[error("unable to unmap dma transaction")]
    Unmap(#[source] anyhow::Error),
}

#[derive(Debug, Copy, Clone)]
pub struct MapDmaOptions {
    pub always_bounce: bool,
    pub is_rx: bool,
    pub is_tx: bool,
    // todo?
    // pub non_blocking: bool,
}

// TODO: remove debug bounds, use inspect instead
pub trait MappedDmaTransaction: std::fmt::Debug {
    fn pfns(&self) -> &[u64];
    // TODO: ugly - want consuming but cannot do that with object safe methods.
    fn complete(&self) -> anyhow::Result<()>;

    // TODO: testing only, do not commit
    fn write_bounced(&self, buf: &[u8]) -> anyhow::Result<()>;
}

#[derive(Debug)]
enum MappedDma<'a> {
    Direct(PagedRange<'a>),
    Mapped(Box<dyn MappedDmaTransaction + 'a>),
}

impl MappedDma<'_> {
    /// the mapped ranges for this transaction
    pub fn pfns(&self) -> &[u64] {
        match self {
            MappedDma::Direct(range) => range.gpns(),
            MappedDma::Mapped(mapped) => mapped.pfns(),
        }
    }
}

/// BUGBUG rename, split into separate traits for mapping vs allocation
/// Dma platform implementation
pub trait DmaClient: Send + Sync + Inspect {
    /// Allocate a new DMA buffer. This buffer must be zero initialized.
    ///
    /// TODO: string tag for allocation?
    fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock>;

    /// Attach to a previously allocated memory block.
    fn attach_dma_buffer(&self, len: usize, base_pfn: u64) -> anyhow::Result<MemoryBlock>;

    fn requires_dma_mapping(&self) -> bool;

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
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn MappedDmaTransaction + 'a>, MapDmaError>> + 'a>>;

    /// Unmap a dma transaction to observe the dma into the requested ranges.
    ///
    /// TODO: return original ranges?
    fn unmap_dma_ranges(
        &self,
        transaction: Box<dyn MappedDmaTransaction + '_>,
    ) -> Result<(), MapDmaError>;
}

// BUGBUG RENAME
#[derive(Inspect, Clone)]
pub struct DmaClientDriver {
    #[inspect(flatten)]
    inner: Arc<dyn DmaClient>,
}

impl DmaClientDriver {
    pub fn new(inner: Arc<dyn DmaClient>) -> Self {
        Self { inner }
    }

    /// Allocate a new DMA buffer. This buffer must be zero initialized.
    ///
    /// TODO: string tag for allocation?
    pub fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock> {
        self.inner.allocate_dma_buffer(total_size)
    }

    /// Attach to a previously allocated memory block.
    pub fn attach_dma_buffer(&self, len: usize, base_pfn: u64) -> anyhow::Result<MemoryBlock> {
        self.inner.attach_dma_buffer(len, base_pfn)
    }

    /// Map the given ranges for DMA. A caller must call `unmap_dma_ranges` to
    /// complete a dma transaction to observe the dma in the passed in ranges.
    ///
    /// This function may block, as if a page is required to be bounced it may
    /// block waiting for bounce buffer space to become available.
    pub async fn map_dma_ranges<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Result<MappedDma<'a>, MapDmaError> {
        if self.inner.requires_dma_mapping() {
            let mapped = self
                .inner
                .map_dma_ranges(guest_memory, range, options)
                .await?;
            Ok(MappedDma::Mapped(mapped))
        } else {
            Ok(MappedDma::Direct(range))
        }
    }

    /// Unmap a dma transaction to observe the dma into the requested ranges.
    ///
    /// TODO: return original ranges?
    pub fn unmap_dma_ranges(&self, transaction: MappedDma<'_>) -> Result<(), MapDmaError> {
        match transaction {
            MappedDma::Mapped(transaction) => self.inner.unmap_dma_ranges(transaction),
            MappedDma::Direct(_) => Ok(()),
        }
    }
}
