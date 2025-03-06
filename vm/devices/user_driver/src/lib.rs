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

/// Errors for [`DmaMap`].
#[derive(Debug, thiserror::Error)]
pub enum DmaMapError {
    #[error("failed to map ranges")]
    Map(#[source] anyhow::Error),
    #[error("no bounce buffers available")]
    NoBounceBufferAvailable,
    #[error("mapped range {range_bytes} is larger than available total bounce buffer space")]
    NotEnoughBounceBufferSpace { range_bytes: usize },
    #[error("unable to unmap dma transaction")]
    Unmap(#[source] anyhow::Error),
}

/// Options for [`DmaClient::map_dma_ranges`].
#[derive(Debug, Copy, Clone)]
pub struct MapDmaOptions {
    /// Always bounce this range, even if pinning would be possible.
    pub always_bounce: bool,
    /// This range is a recieve, aka an external entity is expected to write
    /// into this mapped range. The range's initial contents will not be copied to the
    /// transaction returned.
    pub is_rx: bool,
    /// This range is a transmit, aka a driver wants to allow an external entity
    /// to read the data in the range. The range's initial contents will be
    /// copied to the mapped transaction.
    pub is_tx: bool,
}

// BUGBUG: remove debug bounds for testing only
// TODO: inspect bound?
/// Trait implemented by mapped DMA transations, returned by
/// [`DmaMap::map_dma_ranges`].
pub trait MappedDmaTransaction: std::fmt::Debug {
    /// The PFNs for the mapped dma transaction. This may be different from the
    /// original submitted PFNs to map, if the transaction was bounced.
    fn pfns(&self) -> &[u64];
    /// To be used to complete a transaction, by the implementer of [`DmaMap`].
    ///
    /// TODO: Ugly, but the standard drop implementation would not allow
    /// returning errors. Drivers are never given access to this trait as the
    /// public [`DmaClient`] hides this detail, and [`DmaClient`] does not call
    /// this function.
    ///
    /// This is required because downcasting would not be supported if the
    /// caller returns an object with an associated lifetime, and for now avoid
    /// using generics to avoid polluting all drivers with the need to specify a
    /// concrete type.
    ///
    /// Potentially revisit this in the future.
    fn complete(&self) -> anyhow::Result<()>;

    // BUGBUG: testing only, do not commit
    fn write_bounced(&self, buf: &[u8]) -> anyhow::Result<()>;
}

/// A mapped DMA transaction. The caller must call
/// [`DmaClient::unmap_dma_ranges`] to observe the dma.
// BUGBUG remove debug derive, testing only
#[derive(Debug)]
pub struct MappedDma<'a>(MappedDmaInner<'a>);

/// The inner enum type that hides implementation details to callers.
#[derive(Debug)]
enum MappedDmaInner<'a> {
    Direct(PagedRange<'a>),
    Mapped(Box<dyn MappedDmaTransaction + 'a>),
}

impl MappedDma<'_> {
    /// The pfns for this transaction.
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
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn MappedDmaTransaction + 'a>, DmaMapError>> + 'a>>;

    /// Unmap a dma transaction to observe the dma into the requested ranges.
    fn unmap_dma_ranges(
        &self,
        transaction: Box<dyn MappedDmaTransaction + '_>,
    ) -> Result<(), DmaMapError>;
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
    ) -> Result<MappedDma<'a>, DmaMapError> {
        if let Some(map) = &self.map {
            let mapped = map.map_dma_ranges(guest_memory, range, options).await?;
            Ok(MappedDma(MappedDmaInner::Mapped(mapped)))
        } else {
            Ok(MappedDma(MappedDmaInner::Direct(range)))
        }
    }

    /// Unmap a dma transaction to observe the dma into the requested ranges.
    pub fn unmap_dma_ranges(&self, transaction: MappedDma<'_>) -> Result<(), DmaMapError> {
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
