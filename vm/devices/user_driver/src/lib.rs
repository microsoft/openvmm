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
    fn dma_client(&self) -> &DmaClient;

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
    Map(#[source] anyhow::Error),
    #[error("no bounce buffers available")]
    NoBounceBufferAvailable,
    #[error("mapped range {range_bytes} is larger than available total bounce buffer space")]
    NotEnoughBounceBufferSpace { range_bytes: usize },
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

    // BUGBUG: testing only, do not commit
    fn write_bounced(&self, buf: &[u8]) -> anyhow::Result<()>;
}

/// A mapped DMA transaction. The caller must call
/// [`DmaClientDriver::unmap_dma_ranges`] to observe the dma.
#[derive(Debug)]
pub struct MappedDma<'a>(MappedDmaInner<'a>);

/// The inner enum type that hides implementation details to callers.
#[derive(Debug)]
enum MappedDmaInner<'a> {
    Direct(PagedRange<'a>),
    Mapped(Box<dyn MappedDmaTransaction + 'a>),
}

impl MappedDma<'_> {
    /// the mapped ranges for this transaction
    pub fn pfns(&self) -> &[u64] {
        match &self.0 {
            MappedDmaInner::Direct(range) => range.gpns(),
            MappedDmaInner::Mapped(mapped) => mapped.pfns(),
        }
    }

    /// BUGBUG: test only remove
    pub fn write_bounced(&self, buf: &[u8]) -> anyhow::Result<()> {
        match &self.0 {
            MappedDmaInner::Direct(_) => anyhow::bail!("not a bounce buffer"),
            MappedDmaInner::Mapped(mapped) => mapped.write_bounced(buf),
        }
    }
}

/// Trait for allocating DMA buffers and attaching to existing allocations.
pub trait DmaAlloc: Send + Sync + Inspect {
    /// Allocate a new DMA buffer. This buffer must be zero initialized.
    ///
    /// TODO: string tag for allocation?
    fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock>;

    /// Attach to a previously allocated memory block.
    fn attach_dma_buffer(&self, len: usize, base_pfn: u64) -> anyhow::Result<MemoryBlock>;
}

/// Trait for mapping DMA ranges.
pub trait DmaMap: Send + Sync + Inspect {
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
    fn unmap_dma_ranges(
        &self,
        transaction: Box<dyn MappedDmaTransaction + '_>,
    ) -> Result<(), MapDmaError>;
}

/// DMA client used by drivers.
#[derive(Inspect, Clone)]
pub struct DmaClient {
    alloc: Arc<dyn DmaAlloc>,
    map: Option<Arc<dyn DmaMap>>,
}

impl DmaClient {
    /// Create a new DMA client. If `map` is `None`, [`Self::map_dma_ranges`]
    /// and [`Self::unmap_dma_ranges`] will be no-ops.
    pub fn new(alloc: Arc<dyn DmaAlloc>, map: Option<Arc<dyn DmaMap>>) -> Self {
        Self { alloc, map }
    }

    /// Allocate a new DMA buffer. This buffer must be zero initialized.
    ///
    /// TODO: string tag for allocation?
    pub fn allocate_dma_buffer(&self, total_size: usize) -> anyhow::Result<MemoryBlock> {
        self.alloc.allocate_dma_buffer(total_size)
    }

    /// Attach to a previously allocated memory block.
    pub fn attach_dma_buffer(&self, len: usize, base_pfn: u64) -> anyhow::Result<MemoryBlock> {
        self.alloc.attach_dma_buffer(len, base_pfn)
    }

    /// Map the given ranges for DMA. A caller must call
    /// [`Self::unmap_dma_ranges`] to complete a dma transaction to observe the
    /// dma in the passed in ranges.
    ///
    /// This function may block, as if a page is required to be bounced it may
    /// block waiting for bounce buffer space to become available.
    pub async fn map_dma_ranges<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Result<MappedDma<'a>, MapDmaError> {
        if let Some(map) = &self.map {
            let mapped = map.map_dma_ranges(guest_memory, range, options).await?;
            Ok(MappedDma(MappedDmaInner::Mapped(mapped)))
        } else {
            Ok(MappedDma(MappedDmaInner::Direct(range)))
        }
    }

    /// Unmap a dma transaction to observe the dma into the requested ranges.
    pub fn unmap_dma_ranges(&self, transaction: MappedDma<'_>) -> Result<(), MapDmaError> {
        match transaction.0 {
            MappedDmaInner::Mapped(transaction) => self
                .map
                .as_ref()
                .expect("should not have transaction without mapper")
                .unmap_dma_ranges(transaction),
            MappedDmaInner::Direct(_) => Ok(()),
        }
    }
}
