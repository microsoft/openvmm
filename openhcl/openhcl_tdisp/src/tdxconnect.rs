// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
//!
//! Boilerplate scaffolding: the resource-unblock methods are no-op stubs today.
//! The real implementation will issue guest-side TDCALLs (TDG.DMAR.ACCEPT,
//! TDG.TDI.MMIO.ACCEPT, TDG.TDI.START) via the `hcl` `MshvVtl` handle to
//! unblock MMIO and DMA for an attested TDI. What is wired up is
//! [`TdispTdxConnectResourceValidator::probe_tdi`], which issues TDG.TDI.RD
//! against the TDI so a bring-up run can confirm the Connect TDCALLs reach a
//! real interface before the accept paths are implemented.

use crate::TdispResourceValidationInterface;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtl;
use hvdef::Vtl;
use x86defs::tdx::TdCallResult;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdiRdField;
use x86defs::tdx::TdispInterfaceState;
use x86defs::tdx::TdxFunctionId;

/// Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
///
/// After a device has been attested and placed in the Run state, this struct
/// will issue guest-side TDCALLs to the TDX Module to make device resources
/// (MMIO, DMA) accessible to the guest. The unblock methods are currently no-op
/// stubs preceded by [`Self::probe_tdi`]; the accept TDCALLs land in a
/// follow-up change.
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

    /// Build the TDISP `FUNCTION_ID` used to address a TDI in the Connect
    /// TDCALLs.
    ///
    /// `device_id` is the guest device id the host reported for this TDI (see
    /// `TdispVirtualDeviceInterface::tdisp_get_tdi_device_id`), not a VPCI slot
    /// id, and is used directly as the TDISP requester ID. The requester
    /// segment is left zero and marked invalid, which is correct for a
    /// single-segment host; multi-segment support needs the segment plumbed
    /// down from the host alongside the device id.
    fn function_id(device_id: u16) -> TdxFunctionId {
        TdxFunctionId::new()
            .with_requester_id(device_id)
            .with_requester_segment(0)
            .with_segment_valid(false)
    }

    /// Format a failed TDCALL status for tracing, naming the codes that carry
    /// specific meaning for a TDI read.
    fn describe_status(result: TdCallResult) -> String {
        let code = result.code();
        let meaning = match code {
            TdCallResultCode::TDI_NOT_PRESENT | TdCallResultCode::TDI_INVALID_METADATA => {
                " (TDI is unbound; its control structure was removed or reassigned)"
            }
            TdCallResultCode::TDI_INVALID_STATE => " (TDI is unbound or in the TDISP error state)",
            TdCallResultCode::OPERAND_INVALID => {
                " (FUNCTION_ID is not valid or the TDI is not assigned to this TD)"
            }
            TdCallResultCode::OPERAND_BUSY => " (retryable)",
            _ => "",
        };

        format!("{code:?}{meaning}, raw rax {:#x}", u64::from(result))
    }

    /// Issue TDG.TDI.RD against `device_id` to confirm the TDX Connect TDCALLs
    /// operate on this TDI.
    ///
    /// This is the capability-plumbing probe. It reads the two field codes that
    /// need no output buffer:
    ///
    /// * `GET_TDISP_VERSION` - the cheapest TDI-scoped call. It exercises
    ///   FUNCTION_ID validation and the TDIMT lookup without walking the DMAR
    ///   table, so it isolates "can we address this TDI at all" from anything
    ///   about its DMA state.
    /// * `GET_TDISP_STATE` - success means the TDI is bound, and the value
    ///   decodes to the TDISP interface state the TDX Module last observed.
    ///
    /// `GET_TDISP_REPORT_HASH` and `GET_DEVICE_ATTESTATION_INFO_HASH` need a 4K
    /// private page for their output and are deliberately not probed here.
    ///
    /// The resource-unblock TDCALLs (TDG.DMAR.ACCEPT, TDG.TDI.MMIO.ACCEPT) land
    /// in a follow-up change.
    fn probe_tdi(&self, device_id: u16) -> anyhow::Result<()> {
        let mshv_vtl = Self::open_mshv_vtl()?;

        // Sleep for 10 seconds to allow debuggers to see what we're about to do.
        tracing::info!(
            vtom = self.vtom,
            device_id,
            "TDX Connect validator sleeping 10 seconds before issuing TDG.TDI.RD probe"
        );
        std::thread::sleep(std::time::Duration::from_secs(10));
        tracing::info!(
            vtom = self.vtom,
            device_id,
            "TDX Connect validator issuing TDG.TDI.RD probe"
        );

        // The Connect leaves are only present on a TD that enabled the feature,
        // so check before issuing one rather than diagnosing a fault later.
        let config_flags = mshv_vtl.tdx_get_config_flags();
        tracing::info!(
            vtom = self.vtom,
            tdx_connect = config_flags.tdx_connect(),
            page_release = config_flags.page_release(),
            "TDX Connect validator issued TDG.VM.RD(CONFIG_FLAGS)"
        );
        if !config_flags.tdx_connect() {
            anyhow::bail!("TDX Connect is not enabled on this TD; cannot issue TDI TDCALLs");
        }

        let function_id = Self::function_id(device_id);

        let version = mshv_vtl
            .tdx_tdi_rd(function_id, TdiRdField::GET_TDISP_VERSION, 0)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.RD(GET_TDISP_VERSION) failed for requester id {device_id:#x}: {}",
                    Self::describe_status(e)
                )
            })?;

        let state = mshv_vtl
            .tdx_tdi_rd(function_id, TdiRdField::GET_TDISP_STATE, 0)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.RD(GET_TDISP_STATE) failed for requester id {device_id:#x}: {}",
                    Self::describe_status(e)
                )
            })?;

        tracing::info!(
            requester_id = device_id,
            tdisp_version = version,
            tdisp_state = ?TdispInterfaceState(state),
            "TDX Connect validator reached TDI via TDG.TDI.RD"
        );

        Ok(())
    }
}

impl TdispResourceValidationInterface for TdispTdxConnectResourceValidator {
    #[tracing::instrument(skip(self), fields(device_id))]
    fn on_pre_bind(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // Nothing for the TD to do before the host binds the TDI: until the
        // bind completes there is no TDI control structure for the Connect
        // TDCALLs to address.
        tracing::info!(?target_vtl, device_id, "TDX Connect on_pre_bind: no-op");
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn on_pre_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // The TDI is bound but not yet running, which is the first point in the
        // flow where TDG.TDI.RD should succeed. Probe it to confirm the Connect
        // TDCALLs reach this TDI.
        self.probe_tdi(device_id)?;

        // TEST SCAFFOLDING: the probe succeeded, but deliberately fail the
        // attestation so the device is not started. Remove this once the accept
        // TDCALLs are implemented and starting is safe.
        tracing::warn!(
            ?target_vtl,
            device_id,
            "TDX Connect on_pre_start: TDG.TDI.RD probe succeeded; failing attestation on purpose so the TDI is not started"
        );
        anyhow::bail!(
            "TDX Connect on_pre_start: TDG.TDI.RD probe succeeded for requester id {device_id:#x}, \
             but start is intentionally blocked while the accept TDCALLs are unimplemented"
        );
    }

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
        self.probe_tdi(device_id)?;
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
        self.probe_tdi(device_id)?;
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
