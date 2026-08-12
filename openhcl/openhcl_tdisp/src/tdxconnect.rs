// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Intel TDX Connect implementation of [`TdispResourceValidationInterface`].
//!
//! Boilerplate scaffolding: the resource-unblock methods are no-op stubs today.
//! The real implementation will issue guest-side TDCALLs (TDG.DMAR.ACCEPT,
//! TDG.TDI.MMIO.ACCEPT) via the `hcl` `MshvVtl` handle to unblock MMIO and DMA
//! for an attested TDI. What is wired up is the attestation gate: TDG.TDI.RD
//! probes the TDI (including the two hash field codes that write into a private
//! page the validator owns), TDG.TDI.START authorizes the host to start it, and
//! a post-start TDG.TDI.RD confirms the TDX Module sees it in TDISP RUN.

use crate::TdispResourceValidationInterface;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtl;
use hvdef::HV_PAGE_SIZE;
use hvdef::Vtl;
use parking_lot::Mutex;
use user_driver::DmaClient;
use user_driver::lockmem::LockedMemorySpawner;
use user_driver::memory::MemoryBlock;
use x86defs::tdx::TdCallResult;
use x86defs::tdx::TdCallResultCode;
use x86defs::tdx::TdiRdField;
use x86defs::tdx::TdispInterfaceState;
use x86defs::tdx::TdxFunctionId;

/// The TDG.TDI.RD output buffer is a single 4K private page.
const TDI_HASH_BUF_SIZE: usize = HV_PAGE_SIZE as usize;

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
    /// Private VTL2 page handed to TDG.TDI.RD in R8 as the output buffer for
    /// the hash field codes. The mutex serializes the write-then-read against
    /// concurrent probes on other TDIs.
    hash_buf: Mutex<MemoryBlock>,
    /// GPA of `hash_buf`'s single page, resolved once at construction.
    hash_buf_gpa: u64,
}

impl TdispTdxConnectResourceValidator {
    /// Create a new TDX Connect resource validator.
    ///
    /// The signature mirrors `TdispSevTioResourceValidator::new` so the
    /// selection site in `underhill_core` is identical in shape.
    ///
    /// Allocates the TDG.TDI.RD hash output page, so a TD without TDX Connect
    /// enabled also pays for it.
    ///
    /// * `vtom` - The address mask with the VTOM bit set to signify where VTOM
    ///   addresses start in the CVM.
    pub fn new(vtom: u64) -> anyhow::Result<Self> {
        use anyhow::Context;

        // Ordinary VTL2 RAM is what TDG.TDI.RD wants for its output buffer: on
        // a TD it is private, below VTOM, and accepted at boot, and the
        // paravisor kernel's PFNs are TD GPA PFNs.
        let hash_buf = LockedMemorySpawner
            .allocate_dma_buffer(TDI_HASH_BUF_SIZE)
            .context("failed to allocate the TDG.TDI.RD hash output page")?;

        // Handing the TDX Module a shared or misaligned GPA fails the TDCALL
        // with a status that is hard to attribute back to the buffer.
        anyhow::ensure!(
            hash_buf.pfn_bias() == 0,
            "TDG.TDI.RD hash output page has a nonzero pfn bias {:#x}, so it is shared, not private",
            hash_buf.pfn_bias()
        );
        anyhow::ensure!(
            hash_buf.offset_in_page() == 0,
            "TDG.TDI.RD hash output page is not page aligned: offset {:#x}",
            hash_buf.offset_in_page()
        );
        let &[pfn] = hash_buf.pfns() else {
            anyhow::bail!(
                "TDG.TDI.RD hash output page resolved to {} pfns, expected exactly one",
                hash_buf.pfns().len()
            );
        };
        let hash_buf_gpa = pfn * HV_PAGE_SIZE;
        if vtom != 0 {
            anyhow::ensure!(
                hash_buf_gpa < vtom,
                "TDG.TDI.RD hash output page gpa {hash_buf_gpa:#x} is at or above vtom {vtom:#x}"
            );
        }

        // Trace the GPA once: it is what a debugger needs to watch the page
        // while the probe runs.
        tracing::info!(vtom, hash_buf_gpa, "allocated TDG.TDI.RD hash output page");

        Ok(Self {
            vtom,
            hash_buf: Mutex::new(hash_buf),
            hash_buf_gpa,
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
        let code = result.code();
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
            TdCallResultCode::OPERAND_BUSY => " (retryable)",
            _ => "",
        };

        format!("{code:?}{meaning}, raw rax {:#x}", u64::from(result))
    }

    /// Issue TDG.TDI.RD for one of the hash field codes, which write their
    /// result into a private page instead of returning it in RCX. `field` must
    /// be `GET_TDISP_REPORT_HASH` or `GET_DEVICE_ATTESTATION_INFO_HASH`, the
    /// only two codes that take a nonzero output buffer gpa.
    fn read_hash(
        &self,
        mshv_vtl: &MshvVtl,
        function_id: TdxFunctionId,
        field: TdiRdField,
    ) -> anyhow::Result<Vec<u8>> {
        use anyhow::Context;

        let hash_buf = self.hash_buf.lock();

        // The page is reused across TDIs and both field codes, and the TDX
        // Module writes only the hash bytes, so zero it first.
        hash_buf.write_zeros(0, TDI_HASH_BUF_SIZE);

        let hash_len = mshv_vtl
            .tdx_tdi_rd(function_id, field, self.hash_buf_gpa)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.RD({field:?}) failed for requester id {:#x}: {}",
                    function_id.requester_id(),
                    Self::describe_status(e)
                )
            })?;

        // RCX is the hash length for these field codes. Bound it before it
        // becomes a slice length.
        let hash_len = usize::try_from(hash_len)
            .ok()
            .filter(|&len| len != 0 && len <= TDI_HASH_BUF_SIZE)
            .with_context(|| {
                format!("TDG.TDI.RD({field:?}) returned out of range hash length {hash_len}")
            })?;

        let mut hash = vec![0; hash_len];
        hash_buf.read_at(0, &mut hash);
        Ok(hash)
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
    /// It then reads `GET_TDISP_REPORT_HASH` and
    /// `GET_DEVICE_ATTESTATION_INFO_HASH`, which exercise the buffer-carrying
    /// form of the leaf where the TD supplies a private gpa in R8.
    ///
    /// The resource-unblock TDCALLs (TDG.DMAR.ACCEPT, TDG.TDI.MMIO.ACCEPT) land
    /// in a follow-up change.
    fn probe_tdi(&self, mshv_vtl: &MshvVtl, device_id: u16) -> anyhow::Result<()> {
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
            "TDX Connect validator reached TDI via TDG.TDI.RD: requester id {device_id:#x}, TDISP version {version}, TDISP state {:?}",
            TdispInterfaceState(state)
        );

        let report_hash =
            self.read_hash(mshv_vtl, function_id, TdiRdField::GET_TDISP_REPORT_HASH)?;
        let attestation_info_hash = self.read_hash(
            mshv_vtl,
            function_id,
            TdiRdField::GET_DEVICE_ATTESTATION_INFO_HASH,
        )?;

        tracing::info!(
            hash_buf_gpa = self.hash_buf_gpa,
            report_hash_len = report_hash.len(),
            attestation_info_hash_len = attestation_info_hash.len(),
            "TDX Connect validator read TDI hashes via TDG.TDI.RD: requester id {device_id:#x}, \
             TDISP report hash {:02x?}, device attestation info hash {:02x?}",
            report_hash.as_slice(),
            attestation_info_hash.as_slice()
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
        let mshv_vtl = Self::open_mshv_vtl()?;

        // The TDI is bound but not yet running, which is the first point in the
        // flow where TDG.TDI.RD should succeed. Probe it to confirm the Connect
        // TDCALLs reach this TDI.
        self.probe_tdi(&mshv_vtl, device_id)?;

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
        let mshv_vtl = Self::open_mshv_vtl()?;
        let function_id = Self::function_id(device_id);

        // The host says the TDI is running. Per the TDX Connect ABI EAS, the
        // Known RUN state is reliable after TDG.TDI.START, so this is the TDX
        // Module's own view rather than the host's claim.
        let state = mshv_vtl
            .tdx_tdi_rd(function_id, TdiRdField::GET_TDISP_STATE, 0)
            .map_err(|e| {
                anyhow::anyhow!(
                    "TDG.TDI.RD(GET_TDISP_STATE) failed for requester id {device_id:#x} after the host start: {}",
                    Self::describe_status(e)
                )
            })?;

        let state = TdispInterfaceState(state);
        if state != TdispInterfaceState::RUN {
            anyhow::bail!(
                "TDI {device_id:#x} is in TDISP state {state:?} after the host start, expected RUN"
            );
        }

        tracing::info!(
            ?target_vtl,
            device_id,
            "TDX Connect on_post_start: TDI confirmed in TDISP RUN state"
        );

        Ok(())
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
        self.probe_tdi(&Self::open_mshv_vtl()?, device_id)?;
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
        self.probe_tdi(&Self::open_mshv_vtl()?, device_id)?;
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
