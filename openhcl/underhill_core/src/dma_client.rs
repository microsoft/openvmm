use std::sync::{Arc, Weak};
use crate::dma_manager::GlobalDmaManager;
use user_driver::dma;

// DMA Client structure, representing a specific client instance
pub struct DmaClient {
    manager: Weak<GlobalDmaManager>,
}

impl DmaClient {
    pub fn new(manager: Weak<GlobalDmaManager>) -> Self {
        Self { manager }
    }

    fn pin_memory(&self, range: &MemoryRange) -> Result<usize, DmaError> {
        let manager = self.manager.upgrade().ok_or(DmaError::InitializationFailed)?;
        let threshold = manager.get_client_threshold(self).ok_or(DmaError::InitializationFailed)?;

        if range.size <= threshold && manager.is_pinned(range) {
            Ok(range.start)
        } else {
            Err(DmaError::PinFailed)
        }
    }

    pub fn map_dma_ranges(
        &self,
        ranges: &[MemoryRange],
        options: Option<&DmaMapOptions>
    ) -> Result<DmaTransactionHandler, DmaError> {
        let manager = self.manager.upgrade().ok_or(DmaError::InitializationFailed)?;
        let mut dma_transactions = Vec::new();
        let force_bounce_buffer = options.map_or(false, |opts| opts.force_bounce_buffer);

        let threshold = manager.get_client_threshold(self).ok_or(DmaError::InitializationFailed)?;

        for range in ranges {
            let use_bounce_buffer = force_bounce_buffer || range_size > threshold || !self.can_pin(range);


            if use_bounce_buffer {
                // Allocate a bounce buffer for this range
                let bounce_buffer_addr = self
                    .dma_manager
                    .allocate_bounce_buffer(range_size)
                    .map_err(|_| DmaError::BounceBufferFailed)?;

                    dma_transactions.push(DmaTransaction {
                    original_addr: range.start_addr(),
                    dma_addr: bounce_buffer_addr,
                    size: range_size,
                    is_pinned: false,
                    is_bounce_buffer: true,
                    is_physical: false,
                    is_prepinned: false,
                });

                self.copy_to_bounce_buffer(range, bounce_buffer_addr)?;
            } else {
                // Use direct pinning
                let dma_addr = self
                    .dma_manager
                    .pin_memory(range)
                    .map_err(|_| DmaError::PinFailed)?;

                    dma_transactions.push(DmaTransaction {
                    original_addr: range.start_addr(),
                    dma_addr,
                    size: range_size,
                    is_pinned: true,
                    is_bounce_buffer: false,
                    is_physical: true,
                    is_prepinned: false,
                });
            }
        }

        Ok(DmaTransactionHandler { transactions })
    }

    pub fn unmap_dma_ranges(&self, dma_transactions: &[DmaTransaction]) -> Result<(), DmaError> {
        let manager = self.manager.upgrade().ok_or(DmaError::InitializationFailed)?;

        for transaction in dma_transactions {
            if transaction.is_bounce_buffer {
                // Code to release bounce buffer
            } else if transaction.is_pinned && !transaction.is_prepinned {
                // Code to unpin memory
            }
        }
        Ok(())
    }
}


// Implementation of the DMA interface for `DmaClient`
impl DmaInterface for DmaClient {
    fn map_dma_ranges(&self, ranges: &[MemoryRange]) -> Result<DmaTransactionHandler, DmaError> {
        self.map_dma_ranges(ranges)
    }

    fn unmap_dma_ranges(&self, dma_transactions: &[DmaTransaction]) -> Result<(), DmaError> {
        self.unmap_dma_ranges(dma_transactions)
    }
}