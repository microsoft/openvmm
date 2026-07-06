// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock implementations of TDISP traits for use in tests.

use parking_lot::Mutex;

use hvdef::Vtl;

use crate::TdispResourceValidationInterface;

/// Recorded call to [`TdispNoopResourceValidator::tdisp_unblock_mmio`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnblockedMmioRange {
    /// The VTL the MMIO range was unblocked for.
    pub target_vtl: Vtl,
    /// Identifies the TDI device (not a VPCI ID).
    pub device_id: u16,
    /// The base guest physical address of the unblocked MMIO range.
    pub base_gpa: u64,
    /// The offset within `range_id` that unblocking started from.
    pub base_offset: u32,
    /// The length in bytes of the unblocked MMIO range.
    pub length_in_bytes: u32,
    /// Identifies which MMIO range was unblocked.
    pub range_id: u16,
}

/// Mock implementation of [`TdispResourceValidationInterface`] for testing.
///
/// Records calls to [`tdisp_unblock_mmio`] and [`tdisp_unblock_dma`] so tests
/// can verify the correct ranges were unblocked.
///
/// [`tdisp_unblock_mmio`]: TdispResourceValidationInterface::tdisp_unblock_mmio
/// [`tdisp_unblock_dma`]: TdispResourceValidationInterface::tdisp_unblock_dma
#[derive(Default)]
pub struct TdispNoopResourceValidator {
    unblocked_mmio_ranges: Mutex<Vec<UnblockedMmioRange>>,
    dma_unblocked: Mutex<bool>,
}

impl TdispNoopResourceValidator {
    /// Creates a new [`TdispNoopResourceValidator`] with no recorded calls.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the MMIO ranges that were unblocked, in call order.
    pub fn unblocked_mmio_ranges(&self) -> Vec<UnblockedMmioRange> {
        self.unblocked_mmio_ranges.lock().clone()
    }

    /// Returns `true` if [`tdisp_unblock_dma`] was called.
    ///
    /// [`tdisp_unblock_dma`]: TdispResourceValidationInterface::tdisp_unblock_dma
    pub fn dma_unblocked(&self) -> bool {
        *self.dma_unblocked.lock()
    }
}

impl TdispResourceValidationInterface for TdispNoopResourceValidator {
    fn tdisp_unblock_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            ?base_gpa,
            ?base_offset,
            ?length_in_bytes,
            ?range_id,
            "mock resource validator recording MMIO unblock"
        );
        self.unblocked_mmio_ranges.lock().push(UnblockedMmioRange {
            target_vtl,
            device_id,
            range_id,
            base_gpa,
            base_offset,
            length_in_bytes,
        });
        Ok(())
    }

    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "mock resource validator recording DMA unblock"
        );
        *self.dma_unblocked.lock() = true;
        Ok(())
    }

    fn tdisp_block_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            ?base_gpa,
            ?base_offset,
            ?length_in_bytes,
            ?range_id,
            "mock resource validator recording MMIO block"
        );
        self.unblocked_mmio_ranges
            .lock()
            .retain(|r| r.range_id != range_id);
        Ok(())
    }

    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "mock resource validator recording DMA block"
        );
        *self.dma_unblocked.lock() = false;
        Ok(())
    }
}
