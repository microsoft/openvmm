// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A resource validator for platforms that have no resource validation to do.
//!
//! Used on every isolation type without a platform validator of its own, and by
//! the mocked TDISP flow the OpenVMM tests drive. Unblocking and blocking do
//! nothing, but each request is recorded so a test can check which resources
//! the TDISP flow asked for.

use parking_lot::Mutex;

use hvdef::Vtl;

use crate::TdispResourceValidationInterface;
use crate::TdispTdiState;
use std::future::Future;
use std::pin::Pin;
use tdisp::devicereport::TdiReportStruct;

/// A single MMIO unblock request recorded by the validator.
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
    pub length_in_bytes: u64,
    /// Identifies which MMIO range was unblocked.
    pub range_id: u16,
}

/// A [`TdispResourceValidationInterface`] that validates nothing.
///
/// A device driven through the TDISP flow always has a validator, so this
/// stands in wherever the platform has no resources to validate. Every MMIO and
/// DMA request is recorded, so a test can assert on what the flow asked for.
#[derive(Default)]
pub struct TdispNoopResourceValidator {
    unblocked_mmio_ranges: Mutex<Vec<UnblockedMmioRange>>,
    dma_unblocked: Mutex<bool>,
}

impl TdispNoopResourceValidator {
    /// Creates a validator with nothing recorded yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the MMIO ranges that were unblocked, in call order.
    pub fn unblocked_mmio_ranges(&self) -> Vec<UnblockedMmioRange> {
        self.unblocked_mmio_ranges.lock().clone()
    }

    /// Returns `true` if DMA was unblocked.
    pub fn dma_unblocked(&self) -> bool {
        *self.dma_unblocked.lock()
    }
}

impl TdispResourceValidationInterface for TdispNoopResourceValidator {
    fn on_pre_bind(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "no-op resource validator on_pre_bind"
        );
        Ok(())
    }

    fn on_pre_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "no-op resource validator on_pre_start"
        );
        Ok(())
    }

    fn on_post_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "no-op resource validator on_post_start"
        );
        Ok(())
    }

    fn get_tsm_tdi_state(
        &self,
        target_vtl: Vtl,
        device_id: u16,
    ) -> anyhow::Result<Option<TdispTdiState>> {
        // There is no firmware to ask, so report that it cannot answer
        // rather than inventing a state for callers to check against.
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "no-op resource validator get_tsm_tdi_state"
        );
        Ok(None)
    }

    fn tdisp_set_tdi_report(&self, device_id: u16, _report: &TdiReportStruct) {
        tracing::info!(?device_id, "no-op resource validator tdisp_set_tdi_report");
    }

    fn tdisp_clear_tdi_report(&self, device_id: u16) {
        tracing::info!(
            ?device_id,
            "no-op resource validator tdisp_clear_tdi_report"
        );
    }

    fn tdisp_unblock_mmio<'a>(
        &'a self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u64,
        range_id: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async move {
            tracing::info!(
                ?target_vtl,
                ?device_id,
                ?base_gpa,
                ?base_offset,
                ?length_in_bytes,
                ?range_id,
                "no-op resource validator recording MMIO unblock"
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
        })
    }

    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "no-op resource validator recording DMA unblock"
        );
        *self.dma_unblocked.lock() = true;
        Ok(())
    }

    fn tdisp_block_mmio<'a>(
        &'a self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u64,
        range_id: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async move {
            tracing::info!(
                ?target_vtl,
                ?device_id,
                ?base_gpa,
                ?base_offset,
                ?length_in_bytes,
                ?range_id,
                "no-op resource validator recording MMIO block"
            );
            self.unblocked_mmio_ranges
                .lock()
                .retain(|r| r.range_id != range_id);
            Ok(())
        })
    }

    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            ?target_vtl,
            ?device_id,
            "no-op resource validator recording DMA block"
        );
        *self.dma_unblocked.lock() = false;
        Ok(())
    }
}
