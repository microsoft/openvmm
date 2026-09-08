// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
//!
//! Issues the guest-side TDCALLs that gate a TDI and hand its resources to the
//! guest, through the `hcl` `MshvVtl` handle: TDG.TDI.START authorizes the host
//! to start a bound TDI, a post-start TDG.TDI.RD confirms the TDX Module sees
//! it in TDISP RUN, and TDG.TDI.MMIO.ACCEPT plus TDG.DMAR.ACCEPT accept its
//! MMIO and DMA into the TD.
//!
//! The block direction is asymmetric: TDX Connect gives the guest no inverse
//! for either accept leaf, so re-blocking a resource is the host's job and the
//! block methods here do nothing.

use crate::TdispResourceValidationInterface;
use crate::TdispTdiState;
use anyhow::Context as _;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtl;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::collections::HashMap;
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
///
/// After a device has been attested and placed in the Run state, this struct
/// will issue guest-side TDCALLs to the TDX Module to make device resources
/// (MMIO, DMA) accessible to the guest. Note that the unblock paths are
/// currently stubs that validate the TDI but do not yet issue the accept
/// TDCALLs, so no resource is actually made accessible.
pub struct TdispTdxConnectResourceValidator {
    /// The address mask with the VTOM bit set, signifying where VTOM addresses
    /// start in the CVM. Retained for the forthcoming TDCALL work (shared-GPA
    /// boundary masking) and referenced in the stub trace output.
    vtom: u64,
    /// The MMIO range list from each device's TDI interface report, keyed by
    /// TDI device id and recorded by [`Self::tdisp_set_tdi_report`].
    ///
    /// TDG.TDI.MMIO.ACCEPT addresses a range by its position in this list,
    /// while callers identify a range by its `range_id` field, so the position
    /// has to be looked up here. One validator serves every device, hence the
    /// map.
    tdi_mmio_ranges: Mutex<HashMap<u16, Vec<TdispTdiReportMmioInterfaceInfo>>>,
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
        Ok(Self {
            vtom,
            tdi_mmio_ranges: Mutex::new(HashMap::new()),
        })
    }

    /// Resolve a `range_id` to its position in the device's reported MMIO range
    /// list, which is what TDG.TDI.MMIO.ACCEPT takes as `MMIO_RANGE_IDX`.
    fn mmio_range_index(&self, device_id: u16, range_id: u16) -> anyhow::Result<u16> {
        let ranges = self.tdi_mmio_ranges.lock();
        let ranges = ranges.get(&device_id).with_context(|| {
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
        format!(
            "{}, raw rax {:#x}",
            Self::describe_status_code(result.code()),
            u64::from(result)
        )
    }

    /// Format a bare TDCALL status code, for the leaves whose wrappers return
    /// the code rather than the full `TdCallResult`.
    fn describe_status_code(code: TdCallResultCode) -> String {
        let meaning = match code {
            TdCallResultCode::TDI_NOT_PRESENT | TdCallResultCode::TDI_INVALID_METADATA => {
                " (TDI is unbound; its control structure was removed or reassigned)"
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

    /// Name the L1 Secure EPT leaf state implied by a TDG.MEM.PAGE.ATTR.RD
    /// result, using the state names from the TDX Module ABI spec table
    /// "Secure L1 EPT Entry TDX State Returned by TDX Interface Functions".
    ///
    /// The leaf only exposes the MMIO and PENDING bits, so the blocked states
    /// (MMIO_BLOCKED, BLOCKED) cannot be told apart from their mapped
    /// counterparts here. A page in a state the leaf rejects outright does not
    /// reach this function at all: it produces an EPT violation TD exit
    /// instead of a result.
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

    /// Read back the L1 Secure EPT state of one MMIO page and confirm it is in
    /// the state TDG.MEM.PAGE.ATTR.WR needs before it will create the VTL0
    /// alias: MMIO_MAPPED, at 4K.
    ///
    /// This exists because TDG.MEM.PAGE.ATTR.WR answers a page in the wrong
    /// state with an EPT violation TD exit rather than a status code (ABI spec
    /// 5.5.5.3.3 and 5.5.5.3.4), so the failure surfaces on the host as an
    /// unattributed exit instead of an error here. Reading first turns the
    /// common cases into a message naming the state the page is actually in.
    ///
    /// The read carries a weaker form of the same hazard: per ABI spec
    /// 5.5.4.3.3, TDG.MEM.PAGE.ATTR.RD also causes an EPT violation on a page
    /// that is neither guest-readable nor pending. It does not walk the L2
    /// tree to create an alias, though, so an EPT violation on the read
    /// implicates the L1 entry (the host has not finished TDH.MMIO.MT.SET /
    /// TDH.MMIO.MAP) while one on the write implicates the L2 alias (a missing
    /// non-leaf L2 SEPT page the host must add with TDH.MEM.SEPT.ADD).
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

    /// Issue TDG.TDI.RD for one of the hash field codes, which write their
    /// result into a private page instead of returning it in RCX. `field` must
    /// be `GET_TDISP_REPORT_HASH` or `GET_DEVICE_ATTESTATION_INFO_HASH`, the
    /// only two codes that take a nonzero output buffer gpa.
    /// Fails unless this TD has TDX Connect enabled.
    ///
    /// The TDI-scoped leaves only exist on a TD that turned the feature on, so
    /// callers check first rather than attributing the resulting TDCALL status
    /// back to a missing feature.
    fn ensure_tdx_connect(mshv_vtl: &MshvVtl) -> anyhow::Result<()> {
        if !mshv_vtl.tdx_get_config_flags().tdx_connect() {
            anyhow::bail!("TDX Connect is not enabled on this TD; cannot issue TDI TDCALLs");
        }
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
        let mshv_vtl = Self::open_mshv_vtl()?;

        // The TDI is bound but not yet running, which is the first point in the
        // flow where TDG.TDI.RD should succeed. Probe it to confirm the Connect
        // TDCALLs reach this TDI.
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

        // This only authorizes the start; the TDISP transition to RUN happens
        // on the host's subsequent TDH.TDI.START, which `on_post_start`
        // confirms.
        mshv_vtl
            .tdx_tdi_start(function_id, bind_session)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.START failed for requester id {device_id:#x} at bind session {bind_session:#x}: {}",
                    Self::describe_status(e)
                )
            })?;

        tracing::info!(
            ?target_vtl,
            device_id,
            bind_session,
            "TDX Connect on_pre_start: authorized the TDI start with TDG.TDI.START"
        );

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn on_post_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // The host says the TDI is running. Per the TDX Connect ABI EAS, the
        // Known RUN state is reliable after TDG.TDI.START, so this is the TDX
        // Module's own view rather than the host's claim.
        let state = self.get_tsm_tdi_state(target_vtl, device_id)?;
        if state != Some(TdispTdiState::Run) {
            anyhow::bail!(
                "TDI {device_id:#x} is in TDISP state {state:?} after the host start, expected Run"
            );
        }

        tracing::info!(
            ?target_vtl,
            device_id,
            "TDX Connect on_post_start: TDI confirmed in TDISP RUN state"
        );

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn get_tsm_tdi_state(
        &self,
        target_vtl: Vtl,
        device_id: u16,
    ) -> anyhow::Result<Option<TdispTdiState>> {
        let mshv_vtl = Self::open_mshv_vtl()?;

        // The Connect leaves only exist on a TD that enabled the feature, so a
        // TD without it cannot answer rather than having failed to.
        if !mshv_vtl.tdx_get_config_flags().tdx_connect() {
            tracing::info!(
                ?target_vtl,
                device_id,
                "TDX Connect get_tsm_tdi_state: TDX Connect is not enabled on this TD"
            );
            return Ok(None);
        }

        let function_id = Self::function_id(device_id);

        // GET_TDISP_STATE returns its value in RCX. Only the two hash field
        // codes take an output buffer, so the gpa argument must be zero.
        let raw = match mshv_vtl.tdx_tdi_rd(function_id, TdiRdField::GET_TDISP_STATE, 0) {
            Ok(raw) => raw,
            Err(e) => {
                // A GET_TDISP_STATE read only succeeds while the TDI is bound,
                // so these three statuses report an unbound TDI rather than a
                // malformed call. That is a state, not a failure to answer:
                // reporting it as `Unlocked` is what lets a caller catch a host
                // claiming this TDI reached Locked or Run when it never bound.
                //
                // TDI_INVALID_STATE is documented as ambiguous between unbound
                // and the TDISP error state. Both mean the TDI is not where the
                // host said it was, so treat it the same way.
                let code = e.code();
                if matches!(
                    code,
                    TdCallResultCode::TDI_NOT_PRESENT
                        | TdCallResultCode::TDI_INVALID_METADATA
                        | TdCallResultCode::TDI_INVALID_STATE
                ) {
                    tracing::info!(
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

        // The encodings do not line up with `TdispTdiState` and TDISP's ERROR
        // has no counterpart there, so this has to be an explicit match rather
        // than a cast. `TdispInterfaceState` is an open enum, hence the
        // catch-all.
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

        tracing::info!(
            ?target_vtl,
            device_id,
            %state,
            "TDX Connect get_tsm_tdi_state: read TDI state from the TDX Module"
        );

        Ok(Some(state))
    }

    #[tracing::instrument(skip(self, report), fields(device_id))]
    fn tdisp_set_tdi_report(&self, device_id: u16, report: &TdiReportStruct) {
        // Only the MMIO range list is needed, to resolve a range_id to the
        // report-relative index TDG.TDI.MMIO.ACCEPT wants.
        let ranges = report.mmio_interface_info.clone();

        tracing::info!(
            "TDX Connect tdisp_set_tdi_report: recorded the TDI MMIO range list: \
             device_id={device_id:#x}, range_count={}, range_ids={:?}",
            ranges.len(),
            ranges.iter().map(|r| r.range_id).collect::<Vec<_>>()
        );

        self.tdi_mmio_ranges.lock().insert(device_id, ranges);
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_clear_tdi_report(&self, device_id: u16) {
        let removed = self.tdi_mmio_ranges.lock().remove(&device_id).is_some();
        tracing::info!(
            "TDX Connect tdisp_clear_tdi_report: dropped the TDI MMIO range list: \
             device_id={device_id:#x}, removed={removed}"
        );
    }

    #[tracing::instrument(skip(self), fields(device_id, range_id, base_offset, length_in_bytes))]
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

            // TDISP TODO: This needs to be refactored a lot more.
            let mshv_vtl = Self::open_mshv_vtl()?;
            Self::ensure_tdx_connect(&mshv_vtl)?;

            // The caller has already told the host to unblock the range, so its
            // pages are ready to be accepted into the TD below.
            tracing::info!(
                "TDX Connect tdisp_unblock_mmio: unblocking the MMIO range: \
                 device_id={device_id:#x}, range_id={range_id}, base_gpa={base_gpa:#x}, \
                 base_offset={base_offset:#x}, length_in_bytes={length_in_bytes:#x}, \
                 target_vtl={target_vtl:?}, vtom={:#x}",
                self.vtom
            );

            let function_id = Self::function_id(device_id);
            // The leaf addresses the range by its position in the interface
            // report's MMIO list, not by the report's `range_id` field, so
            // resolve it against the report recorded at attestation.
            let mmio_range_index = self.mmio_range_index(device_id, range_id)?;
            let base_pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;
            // Both accept loops, `TdgTdiMmioAcceptR9`'s page-count fields, and
            // the diagnostics below are all u32 page counts, so narrow once
            // here rather than casting at each use. A u32 page count reaches
            // 16TiB, so this is a limit of the TDX Connect ABI rather than one
            // this code imposes.
            let page_count = u32::try_from(length_in_bytes / HV_PAGE_SIZE)
                .context("MMIO range is more than u32::MAX pages")?;
            let base_offset_pages = base_offset / HV_PAGE_SIZE as u32;
            let mut already_accepted = 0u32;

            // The host has put the range's pages in MMIO_PENDING, so accept
            // them into the TD before anything else touches their mappings.
            //
            // Accept comes first because TDG.TDI.MMIO.ACCEPT is what moves the
            // L1 Secure EPT entry from MMIO_PENDING to MMIO_MAPPED, and only
            // then can TDG.MEM.PAGE.ATTR.WR create the VTL0 alias below: the
            // L2 states the ABI spec defines for MMIO are L2_MMIO_MAPPED and
            // L2_MMIO_BLOCKED, with no pending counterpart, so there is no
            // state for an alias over a page the TD has not accepted yet. This
            // also matches how ordinary private memory is handled, where
            // `tdcall::accept_pages` accepts first and sets attributes after.
            //
            // TDISP TODO: this accepts one 4K page per call because
            // `tdcall_tdi_mmio_accept` requires it: the leaf reports its resume
            // cursor in R9 and the ioctl cannot read R9 back. EAS 4.3.5.1 says an
            // interrupted accept resumes the TD with only RCX and R9 updated and
            // that "guest TD software is not directly involved", which suggests
            // the restart is transparent and the whole range could be accepted in
            // a single call. Worth collapsing this loop once that is confirmed on
            // hardware, since a 64MB BAR is 16384 ioctls today.
            tracing::info!(
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

                // RANGE_OFFSET is in pages from the start of the MMIO range,
                // not from `base_gpa`.
                let range = TdgTdiMmioAcceptR9::new()
                    .with_range_size(1)
                    .with_range_offset(base_offset_pages + i);

                // The leaf addresses the range by its position in the interface
                // report's MMIO list, not by the report's `range_id` field.
                match mshv_vtl.tdx_tdi_mmio_accept(
                    function_id,
                    gpa_base_and_level,
                    mmio_range_index,
                    range,
                ) {
                    Ok(()) => {}
                    Err(e) if e.code() == TdCallResultCode::PAGE_ALREADY_ACCEPTED => {
                        already_accepted += 1;
                        tracing::info!(
                            "TDG.TDI.MMIO.ACCEPT: page already accepted, skipping: \
                             device_id={device_id:#x}, range_id={range_id}, \
                             page_gpa={page_gpa:#x}"
                        );
                    }
                    Err(e) => {
                        tracing::error!(
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

            tracing::info!(
                "TDX Connect tdisp_unblock_mmio: MMIO range accepted into the TD: \
                 device_id={device_id:#x}, range_id={range_id}, \
                 mmio_range_index={mmio_range_index}, page_count={page_count}, \
                 already_accepted={already_accepted}, newly_accepted={}",
                page_count - already_accepted
            );

            // Grant the range to L2 VM1, which is VTL0, so the guest can
            // actually drive the device. Done page by page, after the pages
            // have been accepted above.
            //
            // Read and write only: MMIO is not executable, so both execute bits
            // stay clear, and the mask leaves them alone rather than writing
            // them. Note `GpaVmAttributesMask` has no `valid` bit to select --
            // bit 15 of the mask is `inv_ept`, not `valid` -- so the mask covers
            // read and write only.
            let vm_attributes = GpaVmAttributes::new()
                .with_valid(true)
                .with_read(true)
                .with_write(true);
            let attributes = TdgMemPageGpaAttr::new().with_l2_vm1(vm_attributes);
            let attributes_mask = GpaVmAttributesMask::new().with_read(true).with_write(true);
            let mask = TdgMemPageAttrWriteR8::new().with_l2_vm1(attributes_mask);

            tracing::info!(
                "TDX Connect tdisp_unblock_mmio: granting the MMIO range to VTL0 with \
                 TDG.MEM.PAGE.ATTR.WR: device_id={device_id:#x}, range_id={range_id}, \
                 page_count={page_count}, first_pfn={base_pfn:#x}, \
                 attributes={attributes:#x?}, mask={mask:#x?}",
            );

            for i in 0..page_count {
                let pfn = base_pfn + u64::from(i);
                let page_gpa = base_gpa + (u64::from(i) << hvdef::HV_PAGE_SHIFT);
                let range = MemoryRange::from_4k_gpn_range(pfn..pfn + 1);

                // Confirm the page reached MMIO_MAPPED before writing its
                // attributes, so a page the accept above did not actually
                // promote fails here by name instead of as an EPT violation
                // TD exit inside TDG.MEM.PAGE.ATTR.WR.
                Self::check_mmio_page_accepted(
                    &mshv_vtl, device_id, range_id, page_gpa, i, page_count,
                )
                .inspect_err(|e| {
                    tracing::error!(
                        "TDG.MEM.PAGE.ATTR.RD preflight failed, not issuing \
                         TDG.MEM.PAGE.ATTR.WR: {e:#}"
                    );
                })?;

                if let Err(code) = mshv_vtl.tdx_set_page_attributes(range, attributes, mask) {
                    let status = Self::describe_status_code(code);
                    tracing::error!(
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

            tracing::info!(
                "TDX Connect tdisp_unblock_mmio: MMIO range granted to VTL0: \
                 device_id={device_id:#x}, range_id={range_id}, page_count={page_count}"
            );

            Ok(())
        })
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        let mshv_vtl = Self::open_mshv_vtl()?;
        Self::ensure_tdx_connect(&mshv_vtl)?;

        // VM_IDX 0 is the non-partitioned TD or L1; 1-3 select L2 VM1-VM3.
        // OpenHCL is the L1 paravisor with the guest in an L2 VM, so the DMA
        // target follows the same `vtl + 1` convention `hcl` uses for VP enter.
        let target = DmarTarget::new().with_vm_idx(target_vtl as u8 + 1);

        tracing::info!(
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
            tracing::error!(
                "TDX Connect tdisp_unblock_dma: TDG.DMAR.ACCEPT failed for device_id={device_id:#x} (vm_idx {}): {}",
                target.vm_idx(),
                e
            );

            return Err(e);
        }

        tracing::info!(
            "TDX Connect tdisp_unblock_dma: DMA accepted: device_id={device_id:#x}, vm_idx={}",
            target.vm_idx()
        );

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id, range_id, base_offset, length_in_bytes))]
    /// Does nothing on TDX Connect.
    ///
    /// TDX Connect gives the guest no way to re-block an MMIO range it has
    /// accepted. The ABI has TDG.TDI.MMIO.ACCEPT but no inverse, so once a
    /// range's pages are MMIO_MAPPED in the L1 Secure EPT the TD cannot walk
    /// that back itself. Releasing them is the host's job, via the TDH-side
    /// teardown that follows the unbind the caller has already sent.
    ///
    /// This is therefore a permanent property of the platform rather than
    /// something left to implement.
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

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // TDISP TODO: this should call TDG.DMAR.RELEASE (ABI EAS 0.61 4.3.2),
        // which still needs a `DMAR_RELEASE` entry in `TdCallLeaf` plus
        // `tdcall_dmar_release` / `MshvVtl::tdx_dmar_release`. The EAS does not
        // give the leaf number and the other Connect leaves came from the TDX
        // Module Base Spec, so the number has to be confirmed first.
        //
        // Two things to settle along with it: the leaf requires the PASID table
        // entry to be DMAR_PRESENT and is defined as the first step of L1 DMA
        // *reassignment*, and on success it TD-exits with TDX_TD_INV_REQUEST for
        // the VMM to service before the call returns. For a plain unbind the
        // teardown may instead be host-driven via TDH.DMAR.BLOCK.
        tracing::info!(
            vtom = self.vtom,
            ?target_vtl,
            device_id,
            "TDX Connect tdisp_block_dma: no-op stub"
        );
        Ok(())
    }
}
