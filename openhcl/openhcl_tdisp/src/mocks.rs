// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Mock implementations of TDISP traits for use in tests.

use parking_lot::Mutex;

use hvdef::Vtl;

use crate::TdispResourceValidationInterface;

/// Recorded call to [`TdispMockResourceValidator::tdisp_unblock_mmio`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)]
pub struct UnblockedMmioRange {
    pub target_vtl: Vtl,
    pub device_id: u16,
    pub base_gpa: u64,
    pub base_offset: u32,
    pub length_in_bytes: u32,
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
pub struct TdispMockResourceValidator {
    unblocked_mmio_ranges: Mutex<Vec<UnblockedMmioRange>>,
    dma_unblocked: Mutex<bool>,
}

impl TdispMockResourceValidator {
    /// Creates a new [`TdispMockResourceValidator`] with no recorded calls.
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

impl TdispResourceValidationInterface for TdispMockResourceValidator {
    fn tdisp_unblock_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> anyhow::Result<()> {
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

    fn tdisp_unblock_dma(&self, _target_vtl: Vtl, _device_id: u16) -> anyhow::Result<()> {
        *self.dma_unblocked.lock() = true;
        Ok(())
    }
}
