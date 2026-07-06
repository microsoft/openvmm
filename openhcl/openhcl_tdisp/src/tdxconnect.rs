// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
//!
//! Boilerplate scaffolding: the resource-unblock methods are no-op stubs today.
//! The real implementation will issue guest-side TDCALLs (TDG.DMAR.ACCEPT,
//! TDG.TDI.MMIO.ACCEPT, TDG.TDI.VALIDATE, TDG.TDI.START, TDG.TDI.RD) via the
//! `hcl` `MshvVtl` handle to unblock MMIO and DMA for an attested TDI.

use crate::TdispResourceValidationInterface;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtl;
use hvdef::Vtl;

/// Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
///
/// After a device has been attested and placed in the Run state, this struct
/// will issue guest-side TDCALLs to the TDX Module to make device resources
/// (MMIO, DMA) accessible to the guest. The methods are currently no-op stubs;
/// the TDCALL emission lands in a follow-up change.
pub struct TdispTdxConnectResourceValidator {
    /// The address mask with the VTOM bit set, signifying where VTOM addresses
    /// start in the CVM. Retained for the forthcoming TDCALL work (shared-GPA
    /// boundary masking) and referenced in the stub trace output.
    vtom: u64,
}

impl TdispTdxConnectResourceValidator {
    /// Create a new TDX Connect resource validator.
    ///
    /// The signature mirrors `TdispSevTioResourceValidator::new` so the
    /// selection site in `underhill_core` is identical in shape.
    ///
    /// * `vtom` - The address mask with the VTOM bit set to signify where VTOM
    ///   addresses start in the CVM.
    pub fn new(vtom: u64) -> anyhow::Result<Self> {
        Ok(Self { vtom })
    }

    /// Open a fresh `MshvVtl` handle for a single request. Mirrors
    /// `TdispSevTioResourceValidator::open_mshv_vtl`: the handle must be created
    /// on the VP that will service the request, so it is not cached on the
    /// validator.
    fn open_mshv_vtl() -> anyhow::Result<MshvVtl> {
        use anyhow::Context;
        let mshv = Mshv::new().context("failed to create mshv")?;
        let mshv_vtl = mshv.create_vtl().context("failed to create mshv vtl")?;
        Ok(mshv_vtl)
    }

    /// Issue a benign guest TDCALL (`TDG.VM.RD` of CONFIG_FLAGS) to confirm the
    /// paravisor can reach the TDX Module from this validator, logging whether
    /// TDX Connect is enabled on this TD.
    ///
    /// This is the capability-plumbing probe; the resource-unblock TDCALLs
    /// (TDG.DMAR.ACCEPT, TDG.TDI.MMIO.ACCEPT, ...) land in a follow-up change.
    fn probe_tdx_connect(&self) -> anyhow::Result<()> {
        let mshv_vtl = Self::open_mshv_vtl()?;
        let config_flags = mshv_vtl.tdx_get_config_flags();
        tracing::info!(
            vtom = self.vtom,
            tdx_connect = config_flags.tdx_connect(),
            page_release = config_flags.page_release(),
            "TDX Connect validator issued TDG.VM.RD(CONFIG_FLAGS)"
        );
        Ok(())
    }
}

impl TdispResourceValidationInterface for TdispTdxConnectResourceValidator {
    #[tracing::instrument(skip(self), fields(device_id, range_id, base_offset, length_in_bytes))]
    fn tdisp_unblock_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> anyhow::Result<()> {
        self.probe_tdx_connect()?;
        tracing::info!(
            vtom = self.vtom,
            ?target_vtl,
            device_id,
            base_gpa,
            base_offset,
            length_in_bytes,
            range_id,
            "TDX Connect tdisp_unblock_mmio: no-op stub"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        self.probe_tdx_connect()?;
        tracing::info!(
            vtom = self.vtom,
            ?target_vtl,
            device_id,
            "TDX Connect tdisp_unblock_dma: no-op stub"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id, range_id, base_offset, length_in_bytes))]
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
            vtom = self.vtom,
            ?target_vtl,
            device_id,
            base_gpa,
            base_offset,
            length_in_bytes,
            range_id,
            "TDX Connect tdisp_block_mmio: no-op stub"
        );
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracing::info!(
            vtom = self.vtom,
            ?target_vtl,
            device_id,
            "TDX Connect tdisp_block_dma: no-op stub"
        );
        Ok(())
    }
}
