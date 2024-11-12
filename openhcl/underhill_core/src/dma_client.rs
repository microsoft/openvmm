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
    ) -> Result<DmaTransactionHandler, DmaError> {
        let manager = self.manager.upgrade().ok_or(DmaError::InitializationFailed)?;
        let mut dma_transactions = Vec::new();

        let threshold = manager.get_client_threshold(self).ok_or(DmaError::InitializationFailed)?;

        for range in ranges {
            let (dma_addr, is_pinned, is_bounce_buffer) = if range.size <= threshold {
                match self.pin_memory(range) {
                    Ok(pinned_addr) => (pinned_addr, true, false),
                    Err(_) => {
                        let bounce_addr = manager.allocate_bounce_buffer(range.size)?;
                        (bounce_addr, false, true)
                    }
                }
            } else {
                let bounce_addr = manager.allocate_bounce_buffer(range.size)?;
                (bounce_addr, false, true)
            };

            dma_transactions.push(DmaTransaction {
                original_addr: range.start,
                dma_addr,
                size: range.size,
                is_pinned,
                is_bounce_buffer,
                is_physical: !is_bounce_buffer,
                is_prepinned: manager.is_pinned(range),
            });
        }

        Ok(DmaTransactionHandler {
            transactions: dma_transactions,
        })
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