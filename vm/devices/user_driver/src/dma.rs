
#[derive(Debug)]
pub enum DmaError {
    InitializationFailed,
    MapFailed,
    UnmapFailed,
    PinFailed,
    BounceBufferFailed,
}

#[derive(Debug, Clone)]
pub struct DmaMapOptions {
    pub force_bounce_buffer: bool, // Always use bounce buffers, even if pinning succeeds
}

// Structure encapsulating the result of a DMA mapping operation
pub struct DmaTransactionHandler {
    pub transactions: Vec<DmaTransaction>,
}

// Structure representing a DMA transaction with address and metadata
pub struct DmaTransaction {
    pub original_addr: usize,
    pub dma_addr: usize,
    pub size: usize,
    pub is_pinned: bool,
    pub is_bounce_buffer: bool,
    pub is_physical: bool,
    pub is_prepinned: bool,
}

// Trait for the DMA interface
pub trait DmaInterface {
    fn map_dma_ranges(&self, ranges: &[MemoryRange], options: Option<&DmaMapOptions>,) -> Result<DmaTransactionHandler, DmaError>;
    fn unmap_dma_ranges(&self, dma_transactions: &[DmaTransaction]) -> Result<(), DmaError>;
}