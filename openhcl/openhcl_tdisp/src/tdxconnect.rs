// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
//!
//! The TSM on Intel TDX is implemented by the TDX Module, which enforces the
//! security and resource management policies for TDs. The guest makes calls
//! directly to the TDX Module in the form of "TDCALL" instructions dispatched
//! through mshv driver ioctls.

use crate::TdispResourceValidationInterface;
use crate::TdispTdiState;
use anyhow::Context as _;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtl;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::future::Future;
use std::pin::Pin;
use tdisp::devicereport::TdiReportStruct;
use tdisp::devicereport::TdispTdiReportMmioInterfaceInfo;
use x86defs::tdx::DmarTarget;
use x86defs::tdx::GpaVmAttributes;
use x86defs::tdx::GpaVmAttributesMask;
use x86defs::tdx::TdCallResult;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdgMemPageAttrGpaMappingReadRcxResult;
use x86defs::tdx::TdgMemPageAttrWriteR8;
use x86defs::tdx::TdgMemPageGpaAttr;
use x86defs::tdx::TdgMemPageLevel;
use x86defs::tdx::TdgTdiMmioAcceptR9;
use x86defs::tdx::TdgTdiMmioAcceptRcx;
use x86defs::tdx::TdiRdField;
use x86defs::tdx::TdispInterfaceState;
use x86defs::tdx::TdxFunctionId;

/// Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
pub struct TdispTdxConnectResourceValidator {
    /// The address mask with the VTOM bit set, signifying where VTOM addresses
    /// start in the CVM.
    vtom: u64,

    /// The MMIO range list from the TDI interface report, or `None` until
    /// [`Self::tdisp_set_tdi_report`] records one.
    tdi_mmio_ranges: Mutex<Option<Vec<TdispTdiReportMmioInterfaceInfo>>>,
}

impl TdispTdxConnectResourceValidator {
    /// Create a new TDX Connect resource validator.
    ///
    /// * `vtom` - The address mask with the VTOM bit set to signify where VTOM
    ///   addresses start in the CVM.
    pub fn new(vtom: u64) -> anyhow::Result<Self> {
        Ok(Self {
            vtom,
            tdi_mmio_ranges: Mutex::new(None),
        })
    }

    /// Resolve a `range_id` to its position in the device's reported MMIO range
    /// list, which is what TDG.TDI.MMIO.ACCEPT takes as `MMIO_RANGE_IDX`.
    ///
    /// * `device_id` - The guest device id the host reported for this TDI.
    /// * `range_id` - The identifier of the MMIO range within the device's report.
    fn mmio_range_index(&self, device_id: u16, range_id: u16) -> anyhow::Result<u16> {
        let ranges = self.tdi_mmio_ranges.lock();
        let ranges = ranges.as_ref().with_context(|| {
            format!(
                "no TDI interface report recorded for device {device_id:#x}; cannot resolve the \
                 MMIO range index for range {range_id}"
            )
        })?;

        let index = ranges
            .iter()
            .position(|r| r.range_id == range_id)
            .with_context(|| {
                format!(
                    "range {range_id} has no entry in the TDI interface report for device \
                     {device_id:#x} ({} range(s) reported)",
                    ranges.len()
                )
            })?;

        u16::try_from(index).with_context(|| {
            format!("MMIO range index {index} for range {range_id} does not fit in a u16")
        })
    }

    /// Open a fresh `MshvVtl` handle for a single request. The handle only
    /// works on the VP that created it, so it cannot be cached and shared.
    fn open_mshv_vtl() -> anyhow::Result<MshvVtl> {
        use anyhow::Context;
        let mshv = Mshv::new().context("failed to create mshv")?;
        let mshv_vtl = mshv.create_vtl().context("failed to create mshv vtl")?;
        Ok(mshv_vtl)
    }

    /// Build the TDISP `FUNCTION_ID` that addresses a TDI. The segment is left
    /// zero and invalid, which only suits a single-segment host.
    ///
    /// * `device_id` - The guest device id the host reported for this TDI, used
    ///   directly as the TDISP requester ID.
    fn function_id(device_id: u16) -> TdxFunctionId {
        TdxFunctionId::new()
            .with_requester_id(device_id)
            .with_requester_segment(0)
            .with_segment_valid(false)
    }

    /// Format a failed TDCALL status for tracing, naming the codes that carry
    /// specific meaning for a TDI read.
    fn describe_status(result: TdCallResult) -> String {
        format!(
            "{}, raw rax {:#x}",
            Self::describe_status_code(result.code()),
            u64::from(result)
        )
    }

    /// Format a bare TDCALL status code for the leaves whose wrappers return
    /// the code rather than the full `TdCallResult`.
    fn describe_status_code(code: TdCallResultCode) -> String {
        let meaning = match code {
            TdCallResultCode::TDI_NOT_PRESENT | TdCallResultCode::TDI_INVALID_METADATA => {
                " (TDI is unbound, its control structure was removed or reassigned)"
            }
            TdCallResultCode::TDI_INVALID_STATE => {
                " (TDI is unbound, in the TDISP error state, or not in the state the leaf requires)"
            }
            TdCallResultCode::OPERAND_INVALID => {
                " (FUNCTION_ID is not valid or the TDI is not assigned to this TD)"
            }
            TdCallResultCode::OPERAND_ADDR_RANGE_ERROR => {
                " (the output buffer gpa is outside this TD's private gpa range)"
            }
            TdCallResultCode::PAGE_METADATA_INCORRECT | TdCallResultCode::PAGE_NOT_OWNED_BY_TD => {
                " (the output buffer is not an accepted private page owned by this TD)"
            }
            TdCallResultCode::PAGE_ALREADY_ACCEPTED => {
                " (the MMIO page is already mapped into this TD)"
            }
            TdCallResultCode::MMIO_PAGE_NOT_IN_ASSOC_RANGE => {
                " (the gpa is not inside the MMIO range named by the range id)"
            }
            TdCallResultCode::MMIO_TDI_OWNER_MISMATCH => {
                " (the MMIO page belongs to a different TDI)"
            }
            TdCallResultCode::MMIO_INVALID_HPA_OFFSET | TdCallResultCode::PAGE_SIZE_MISMATCH => {
                " (the host's MMIO mapping does not match what the TD is accepting)"
            }
            TdCallResultCode::DMAR_INVALID_MAPPING_STATE => {
                " (the PASID table entry is not pending accept; during a reassignment this is \
                  retryable until the VMM finishes invalidating)"
            }
            TdCallResultCode::OPERAND_BUSY => " (retryable)",
            _ => "",
        };

        format!("{code:?}{meaning}")
    }

    /// Describe the L1 Secure EPT state returned by a call to PAGE.ATTR.RD.
    /// Blocked states cannot be told apart from mapped ones here.
    fn describe_page_state(mapping: TdgMemPageAttrGpaMappingReadRcxResult) -> String {
        let state = match (mapping.mmio(), mapping.pending()) {
            (true, false) => "MMIO_MAPPED (private MMIO, accepted by the TD)",
            (true, true) => {
                "MMIO_PENDING (private MMIO mapped by the host, not yet accepted by the TD)"
            }
            (false, false) => "MAPPED (ordinary private memory, not MMIO)",
            (false, true) => "PENDING (ordinary private memory, not yet accepted)",
        };

        format!(
            "{state} [mmio={}, pending={}, level={:?}, mapping base gpa={:#x}]",
            mapping.mmio(),
            mapping.pending(),
            mapping.level(),
            mapping.gpa_page_number() << hvdef::HV_PAGE_SHIFT,
        )
    }

    /// Confirm one MMIO page is MMIO_MAPPED at 4K.
    fn check_mmio_page_accepted(
        mshv_vtl: &MshvVtl,
        device_id: u16,
        range_id: u16,
        page_gpa: u64,
        page_index: u32,
        page_count: u32,
    ) -> anyhow::Result<()> {
        let result = mshv_vtl
            .tdx_read_page_attributes(page_gpa)
            .map_err(|code| {
                anyhow::anyhow!(
                    "TDG.MEM.PAGE.ATTR.RD failed for requester id {device_id:#x} range {range_id} \
                     page {page_gpa:#x} (page {page_index} of {page_count}): {}",
                    Self::describe_status_code(code)
                )
            })?;

        let mapping = result.mapping;
        let expected = "MMIO_MAPPED (private MMIO, accepted by the TD) [mmio=true, \
                        pending=false, level=Size4k]";

        if !mapping.mmio() || mapping.pending() || mapping.level() != TdgMemPageLevel::Size4k {
            anyhow::bail!(
                "MMIO page {page_gpa:#x} for requester id {device_id:#x} range {range_id} \
                 (page {page_index} of {page_count}) is in the wrong L1 Secure EPT state for \
                 TDG.MEM.PAGE.ATTR.WR.\n  found:    {}\n  expected: {expected}\n  \
                 attributes: {:#x?}",
                Self::describe_page_state(mapping),
                result.attributes,
            );
        }

        Ok(())
    }

    /// Fail unless this TD has TDX Connect enabled, since the TDI-scoped
    /// leaves do not exist without it.
    fn ensure_tdx_connect(mshv_vtl: &MshvVtl) -> anyhow::Result<()> {
        if !mshv_vtl.tdx_get_config_flags().tdx_connect() {
            anyhow::bail!("TDX Connect is not enabled on this TD; cannot issue TDI TDCALLs");
        }
        Ok(())
    }
}

impl TdispResourceValidationInterface for TdispTdxConnectResourceValidator {
    fn on_pre_bind(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // Nothing to do before the bind.
        tracelimit::info_ratelimited!(?target_vtl, device_id, "TDX Connect on_pre_bind: no-op");
        Ok(())
    }

    fn on_pre_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        let mshv_vtl = Self::open_mshv_vtl()?;

        // The TDI is bound but not running, the first point where TDG.TDI.RD
        // should succeed.
        Self::ensure_tdx_connect(&mshv_vtl)?;

        let function_id = Self::function_id(device_id);

        // TDG.TDI.START checks EXP_BIND_SESSION against TDI_CS.BIND_SESSION_ID,
        // so read the session the TDX Module currently has for this TDI.
        let bind_session = mshv_vtl
            .tdx_tdi_rd(function_id, TdiRdField::GET_BIND_SESSION_ID, 0)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.RD(GET_BIND_SESSION_ID) failed for requester id {device_id:#x}: {}",
                    Self::describe_status(e)
                )
            })?;

        // This only authorizes the start, the host's later call to
        // TDH.TDI.START performs the transition to RUN.
        mshv_vtl
            .tdx_tdi_start(function_id, bind_session)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.START failed for requester id {device_id:#x} at bind session {bind_session:#x}: {}",
                    Self::describe_status(e)
                )
            })?;

        tracelimit::info_ratelimited!(
            ?target_vtl,
            device_id,
            bind_session,
            "TDX Connect on_pre_start: authorized the TDI start with TDG.TDI.START"
        );

        Ok(())
    }

    fn on_post_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // Nothing to do after the start.
        tracelimit::info_ratelimited!(?target_vtl, device_id, "TDX Connect on_post_start: no-op");

        Ok(())
    }

    fn get_tsm_tdi_state(
        &self,
        target_vtl: Vtl,
        device_id: u16,
    ) -> anyhow::Result<Option<TdispTdiState>> {
        let mshv_vtl = Self::open_mshv_vtl()?;
        let function_id = Self::function_id(device_id);

        // Output buffer gpa must be zero since GET_TDISP_STATE returns its
        // value directly in a register.
        let raw = match mshv_vtl.tdx_tdi_rd(function_id, TdiRdField::GET_TDISP_STATE, 0) {
            Ok(raw) => raw,
            Err(e) => {
                // These statuses unambiguously mean the TDI is unbound, so
                // report it as `Unlocked`. TDI_INVALID_STATE is deliberately
                // excluded, as it also covers the TDISP error state.
                let code = e.code();
                if matches!(
                    code,
                    TdCallResultCode::TDI_NOT_PRESENT | TdCallResultCode::TDI_INVALID_METADATA
                ) {
                    tracelimit::info_ratelimited!(
                        ?target_vtl,
                        device_id,
                        status = %Self::describe_status(e),
                        "TDX Connect get_tsm_tdi_state: TDI is unbound, reporting Unlocked"
                    );
                    return Ok(Some(TdispTdiState::Unlocked));
                }

                anyhow::bail!(
                    "TDG.TDI.RD(GET_TDISP_STATE) failed for requester id {device_id:#x}: {}",
                    Self::describe_status(e)
                );
            }
        };

        // The encodings do not line up with `TdispTdiState` and ERROR has no
        // counterpart, so map them explicitly.
        let state = TdispInterfaceState(raw);
        let state = match state {
            TdispInterfaceState::CONFIG_UNLOCKED => TdispTdiState::Unlocked,
            TdispInterfaceState::CONFIG_LOCKED => TdispTdiState::Locked,
            TdispInterfaceState::RUN => TdispTdiState::Run,
            TdispInterfaceState::ERROR => anyhow::bail!(
                "TDI {device_id:#x} is in the TDISP error state, which has no TDI state equivalent"
            ),
            other => anyhow::bail!(
                "TDG.TDI.RD(GET_TDISP_STATE) returned unknown TDISP state {other:?} for requester id {device_id:#x}"
            ),
        };

        tracelimit::info_ratelimited!(
            ?target_vtl,
            device_id,
            %state,
            "TDX Connect get_tsm_tdi_state: read TDI state from the TDX Module"
        );

        Ok(Some(state))
    }

    fn tdisp_set_tdi_report(&self, device_id: u16, report: &TdiReportStruct) {
        // The MMIO range list is needed to resolve a range_id to the
        // report-relative index TDG.TDI.MMIO.ACCEPT wants.
        let ranges = report.mmio_interface_info.clone();

        tracelimit::info_ratelimited!(
            "TDX Connect tdisp_set_tdi_report: recorded the TDI MMIO range list: \
             device_id={device_id:#x}, range_count={}, range_ids={:?}",
            ranges.len(),
            ranges.iter().map(|r| r.range_id).collect::<Vec<_>>()
        );

        *self.tdi_mmio_ranges.lock() = Some(ranges);
    }

    fn tdisp_clear_tdi_report(&self, device_id: u16) {
        let removed = self.tdi_mmio_ranges.lock().take().is_some();
        tracelimit::info_ratelimited!(
            "TDX Connect tdisp_clear_tdi_report: dropped the TDI MMIO range list: \
             device_id={device_id:#x}, removed={removed}"
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
            if length_in_bytes == 0 {
                anyhow::bail!("length_in_bytes must be greater than 0");
            }
            if !length_in_bytes.is_multiple_of(HV_PAGE_SIZE) {
                anyhow::bail!("length_in_bytes must be page aligned");
            }

            let mshv_vtl = Self::open_mshv_vtl()?;
            Self::ensure_tdx_connect(&mshv_vtl)?;

            // The caller has already told the host to unblock the range, so its
            // pages are ready to be accepted into the TD below.
            tracelimit::info_ratelimited!(
                "TDX Connect tdisp_unblock_mmio: unblocking the MMIO range: \
                 device_id={device_id:#x}, range_id={range_id}, base_gpa={base_gpa:#x}, \
                 base_offset={base_offset:#x}, length_in_bytes={length_in_bytes:#x}, \
                 target_vtl={target_vtl:?}, vtom={:#x}",
                self.vtom
            );

            let function_id = Self::function_id(device_id);

            // The leaf addresses the range by its position in the report's MMIO
            // list, not by the report's `range_id` field. Convert it here.
            let mmio_range_index = self.mmio_range_index(device_id, range_id)?;
            let base_pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;

            // Narrow once here as the accept loops and the diagnostics below
            // all take u32 page counts.
            let page_count = u32::try_from(length_in_bytes / HV_PAGE_SIZE)
                .context("MMIO range is more than u32::MAX pages")?;
            let base_offset_pages = base_offset / HV_PAGE_SIZE as u32;
            let mut already_accepted = 0u32;

            // Accept the pages first, moving them from MMIO_PENDING to
            // MMIO_MAPPED so the VTL0 alias below can be created.
            //
            // TDISP TODO: this accepts one 4K page per call because
            // `tdcall_tdi_mmio_accept` requires it. When OpenHCL Linux's tdcall
            // interface supports the necessary batch accept mechanism, this
            // loop can be optimized.
            tracelimit::info_ratelimited!(
                "TDX Connect tdisp_unblock_mmio: accepting MMIO pages with TDG.TDI.MMIO.ACCEPT: \
                 device_id={device_id:#x}, range_id={range_id}, \
                 mmio_range_index={mmio_range_index}, page_count={page_count}, \
                 base_offset_pages={base_offset_pages}, first_pfn={base_pfn:#x}"
            );

            for i in 0..page_count {
                let page_gpa = base_gpa + (u64::from(i) << hvdef::HV_PAGE_SHIFT);

                let gpa_base_and_level = TdgTdiMmioAcceptRcx::new()
                    .with_level(TdgMemPageLevel::Size4k)
                    .with_gpa_page_number(base_pfn + u64::from(i));

                // RANGE_OFFSET counts pages from the start of the MMIO range,
                // not from `base_gpa`.
                let range = TdgTdiMmioAcceptR9::new()
                    .with_range_size(1)
                    .with_range_offset(base_offset_pages + i);

                // NOTE: this tdcall addresses the range by its position in the
                // interface report's MMIO list, not by the report's `range_id`
                // field.
                match mshv_vtl.tdx_tdi_mmio_accept(
                    function_id,
                    gpa_base_and_level,
                    mmio_range_index,
                    range,
                ) {
                    Ok(()) => {}
                    Err(e) if e.code() == TdCallResultCode::PAGE_ALREADY_ACCEPTED => {
                        already_accepted += 1;
                        tracelimit::info_ratelimited!(
                            "TDG.TDI.MMIO.ACCEPT: page already accepted, skipping: \
                             device_id={device_id:#x}, range_id={range_id}, \
                             page_gpa={page_gpa:#x}"
                        );
                    }
                    Err(e) => {
                        tracelimit::error_ratelimited!(
                            "TDG.TDI.MMIO.ACCEPT failed for requester id {device_id:#x} range \
                             {range_id} (report index {mmio_range_index}) page {page_gpa:#x} \
                             (page {i} of {page_count}): {}",
                            Self::describe_status(e)
                        );

                        anyhow::bail!(
                            "TDG.TDI.MMIO.ACCEPT failed for requester id {device_id:#x} range \
                             {range_id} (report index {mmio_range_index}) page {page_gpa:#x} \
                             (page {i} of {page_count}): {}",
                            Self::describe_status(e)
                        );
                    }
                }
            }

            tracelimit::info_ratelimited!(
                "TDX Connect tdisp_unblock_mmio: MMIO range accepted into the TD: \
                 device_id={device_id:#x}, range_id={range_id}, \
                 mmio_range_index={mmio_range_index}, page_count={page_count}, \
                 already_accepted={already_accepted}, newly_accepted={}",
                page_count - already_accepted
            );

            // Grant the range to L2 VM1 (VTL0) so the guest can access the
            // device. RW only as MMIO can never be executable.
            let vm_attributes = GpaVmAttributes::new()
                .with_valid(true)
                .with_read(true)
                .with_write(true);
            let attributes = TdgMemPageGpaAttr::new().with_l2_vm1(vm_attributes);
            let attributes_mask = GpaVmAttributesMask::new().with_read(true).with_write(true);
            let mask = TdgMemPageAttrWriteR8::new().with_l2_vm1(attributes_mask);

            tracelimit::info_ratelimited!(
                "TDX Connect tdisp_unblock_mmio: granting the MMIO range to VTL0 with \
                 TDG.MEM.PAGE.ATTR.WR: device_id={device_id:#x}, range_id={range_id}, \
                 page_count={page_count}, first_pfn={base_pfn:#x}, \
                 attributes={attributes:#x?}, mask={mask:#x?}",
            );

            for i in 0..page_count {
                let pfn = base_pfn + u64::from(i);
                let page_gpa = base_gpa + (u64::from(i) << hvdef::HV_PAGE_SHIFT);
                let range = MemoryRange::from_4k_gpn_range(pfn..pfn + 1);

                // Confirm the page reached MMIO_MAPPED so a failure identifies
                // the page that failed instead of surfacing as an EPT
                // violation.
                Self::check_mmio_page_accepted(
                    &mshv_vtl, device_id, range_id, page_gpa, i, page_count,
                )
                .inspect_err(|e| {
                    tracelimit::error_ratelimited!(
                        "TDG.MEM.PAGE.ATTR.RD preflight failed, not issuing \
                         TDG.MEM.PAGE.ATTR.WR: {e:#}"
                    );
                })?;

                if let Err(code) = mshv_vtl.tdx_set_page_attributes(range, attributes, mask) {
                    let status = Self::describe_status_code(code);
                    tracelimit::error_ratelimited!(
                        "TDG.MEM.PAGE.ATTR.WR failed: device_id={device_id:#x}, \
                         range_id={range_id}, page_gpa={page_gpa:#x} \
                         (page {i} of {page_count}), status={status}"
                    );

                    anyhow::bail!(
                        "TDG.MEM.PAGE.ATTR.WR failed for requester id {device_id:#x} range \
                         {range_id} page {page_gpa:#x} (page {i} of {page_count}): {status}"
                    );
                }
            }

            tracelimit::info_ratelimited!(
                "TDX Connect tdisp_unblock_mmio: MMIO range granted to VTL0: \
                 device_id={device_id:#x}, range_id={range_id}, page_count={page_count}"
            );

            Ok(())
        })
    }

    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        let mshv_vtl = Self::open_mshv_vtl()?;
        Self::ensure_tdx_connect(&mshv_vtl)?;

        // Map the TDX VM index from the VTL value. Example: VM_IDX 0 is L1,
        // which is VTL0.
        let target = DmarTarget::new().with_vm_idx(target_vtl as u8 + 1);

        tracelimit::info_ratelimited!(
            "TDX Connect tdisp_unblock_dma: accepting DMA with TDG.DMAR.ACCEPT: \
             device_id={device_id:#x}, vm_idx={}, target_vtl={target_vtl:?}, vtom={:#x}",
            target.vm_idx(),
            self.vtom
        );

        let err = mshv_vtl
            .tdx_dmar_accept(Self::function_id(device_id), target)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.DMAR.ACCEPT failed for requester id {device_id:#x} (vm_idx {}): {}",
                    target.vm_idx(),
                    Self::describe_status(e)
                )
            });

        if let Err(e) = err {
            tracelimit::error_ratelimited!(
                "TDX Connect tdisp_unblock_dma: TDG.DMAR.ACCEPT failed for device_id={device_id:#x} (vm_idx {}): {}",
                target.vm_idx(),
                e
            );

            return Err(e);
        }

        tracelimit::info_ratelimited!(
            "TDX Connect tdisp_unblock_dma: DMA accepted: device_id={device_id:#x}, vm_idx={}",
            target.vm_idx()
        );

        Ok(())
    }

    /// Does nothing, as TDX Connect gives the guest no inverse for
    /// TDG.TDI.MMIO.ACCEPT. Releasing the pages is the host's job.
    ///
    /// Resource re-blocking is automatically enforced on Unbind by TDXC.
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
            tracelimit::info_ratelimited!(
                vtom = self.vtom,
                ?target_vtl,
                device_id,
                base_gpa,
                base_offset,
                length_in_bytes,
                range_id,
                "TDX Connect tdisp_block_mmio: nothing to do, the guest cannot re-block MMIO"
            );
            Ok(())
        })
    }

    /// Does nothing. Resource re-blocking is automatically enforced on Unbind by TDXC.
    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        tracelimit::info_ratelimited!(
            vtom = self.vtom,
            ?target_vtl,
            device_id,
            "TDX Connect tdisp_block_dma: no-op stub"
        );
        Ok(())
    }
}
