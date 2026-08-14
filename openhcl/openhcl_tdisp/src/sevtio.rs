// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides an implementation of the SEV-TIO resource validation interface for
//! TDISP devices. This is used by OpenHCL devices that are exposed to SEV guests and need to
//! communicate with the SEV firmware to unblock device resources after attestation.

use crate::TdispHostCommandSender;
use crate::TdispResourceValidationInterface;
use anyhow::Context;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvHvcall;
use hcl::ioctl::MshvVtl;
use hvdef::Vtl;
use hvdef::hypercall::HostVisibilityType;
use memory_range::MemoryRange;
use sev_guest_device::SevGuestDevice;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tdisp::devicereport::TdiReportStruct;
use x86defs::snp::SevRmpAdjust;

/// How long to pause before each MMIO-related hypercall. For debug purposes only.
const MMIO_HYPERCALL_PAUSE: Duration = Duration::from_secs(0);

/// Logs exactly which MMIO hypercall is about to be issued, then sleeps for
/// [`MMIO_HYPERCALL_PAUSE`] before the caller performs it. This is to allow
/// attaching a debugger to the VP before the hypercall is issued, so the
/// hypercall can be single-stepped and the RMP state can be inspected before
/// and after the hypercall.
macro_rules! debug_pause_for_breakpoint {
    ($($arg:tt)*) => {{
        if (MMIO_HYPERCALL_PAUSE == Duration::ZERO) {
            tracing::info!(
                pause_secs = MMIO_HYPERCALL_PAUSE.as_secs(),
                "executing hypercall: {}",
                format_args!($($arg)*)
            );
        } else {
            tracing::info!(
                pause_secs = MMIO_HYPERCALL_PAUSE.as_secs(),
                "pausing {}s before MMIO hypercall: {}",
                MMIO_HYPERCALL_PAUSE.as_secs(),
                format_args!($($arg)*)
            );
            std::thread::sleep(MMIO_HYPERCALL_PAUSE);
        }
    }};
}

/// Records the PFNs marked immutable by a
/// `modify_gpa_visibility_and_immutability(.., true, ..)` call, so the
/// immutable bit can be undone if a later step fails before it is cleared on
/// the normal path.
///
/// Armed on construction (i.e. immediately after a successful mark). Call
/// [`Self::disarm`] once immutability has been cleared on the success path so
/// `Drop` does not attempt a second, redundant clear.
struct ImmutablePfnGuard<'a> {
    mshv: &'a MshvHvcall,
    pfns: Vec<u64>,
    armed: bool,
}

impl<'a> ImmutablePfnGuard<'a> {
    fn new(mshv: &'a MshvHvcall, pfns: Vec<u64>) -> Self {
        Self {
            mshv,
            pfns,
            armed: true,
        }
    }

    /// Cancel the rollback after immutability has been cleared normally.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ImmutablePfnGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        tracing::warn!(
            page_count = self.pfns.len(),
            "rolling back immutable bit on PFNs after failed MMIO block/unblock"
        );

        debug_pause_for_breakpoint!(
            "modify_gpa_visibility_and_immutability(PRIVATE, immutable=false) on {} pfn(s) starting at {:#x} to roll back the immutable bit after a failed MMIO block/unblock",
            self.pfns.len(),
            self.pfns.first().copied().unwrap_or(0)
        );

        if let Err((e, processed)) = self.mshv.modify_gpa_visibility_and_immutability(
            HostVisibilityType::PRIVATE,
            false,
            &self.pfns,
        ) {
            // Leaving pages stuck immutable is unrecoverable. Matches the
            // PagesAccessibleToLowerVtl precedent in lower_vtl_permissions_guard.
            panic!(
                "failed to roll back immutable bit on {} PFNs ({processed} cleared before failure): {e:?}",
                self.pfns.len()
            );
        }
    }
}

/// AMD SEV-TIO implementation of [`TdispResourceValidationInterface`].
///
/// After a device has been attested and placed in the Run state, this struct
/// communicates with the SEV firmware via `/dev/sev-guest` and issues
/// hypercalls to make device resources (MMIO, DMA) accessible to the guest.
pub struct TdispSevTioResourceValidator {
    sev_guest: SevGuestDevice,
    vtom: u64,
}

impl TdispSevTioResourceValidator {
    /// Open a handle to the `/dev/sev-guest` device required for SEV-TIO
    /// operations.
    ///
    /// Note: the `mshv` and `mshv_vtl` handles are intentionally not cached
    /// here; they are (re)created on each request so that the VP servicing
    /// the request is the same VP that created the handle.
    ///
    /// * `vtom` - The address mask with the VTOM bit set to signify where VTOM
    ///   addresses start in the CVM.
    pub fn new(vtom: u64) -> anyhow::Result<Self> {
        let sev_guest = SevGuestDevice::open()
            .context("failed to open /dev/sev-guest")
            .unwrap();

        Ok(Self { sev_guest, vtom })
    }

    /// Open a fresh `MshvHvcall` handle for a single request. The handle must
    /// be created on the VP that will use it, so we do not cache it on the
    /// validator.
    fn open_mshv_hvcall() -> anyhow::Result<MshvHvcall> {
        let mshv = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        mshv.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallModifySparseGpaPageHostVisibility,
        ]);
        Ok(mshv)
    }

    /// Open a fresh `MshvVtl` handle for a single request. The handle must be
    /// created on the VP that will use it, so we do not cache it on the
    /// validator.
    fn open_mshv_vtl() -> anyhow::Result<MshvVtl> {
        let mshv_vtl_changer = Mshv::new().context("failed to create mshv")?;
        let mshv_vtl = mshv_vtl_changer
            .create_vtl()
            .context("failed to create mshv vtl")?;
        Ok(mshv_vtl)
    }

    fn vtl_to_vmpl(vtl: Vtl) -> u8 {
        match vtl {
            Vtl::Vtl0 => x86defs::snp::Vmpl::Vmpl2.into(),
            Vtl::Vtl1 => x86defs::snp::Vmpl::Vmpl1.into(),
            Vtl::Vtl2 => x86defs::snp::Vmpl::Vmpl0.into(),
        }
    }
}

impl TdispResourceValidationInterface for TdispSevTioResourceValidator {
    #[tracing::instrument(skip(self), fields(device_id))]
    fn on_pre_bind(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // SEV-TIO has nothing to do before the bind; the PSP work all happens
        // once the device is running and its resources are being unblocked.
        tracing::info!(?target_vtl, device_id, "SEV-TIO on_pre_bind: no-op");
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn on_pre_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // See `on_pre_bind`.
        tracing::info!(?target_vtl, device_id, "SEV-TIO on_pre_start: no-op");
        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn on_post_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // See `on_pre_bind`.
        tracing::info!(?target_vtl, device_id, "SEV-TIO on_post_start: no-op");
        Ok(())
    }

    #[tracing::instrument(skip(self, _report), fields(device_id))]
    fn tdisp_set_tdi_report(&self, device_id: u16, _report: &TdiReportStruct) {
        // SEV-TIO addresses MMIO ranges by range id, so it has no use for the
        // report's list ordering.
        tracing::info!(device_id, "SEV-TIO tdisp_set_tdi_report: no-op");
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_clear_tdi_report(&self, device_id: u16) {
        // See `tdisp_set_tdi_report`.
        tracing::info!(device_id, "SEV-TIO tdisp_clear_tdi_report: no-op");
    }

    #[tracing::instrument(
        skip(self, _host),
        fields(device_id, range_id, base_offset, length_in_bytes)
    )]
    fn tdisp_unblock_mmio<'a>(
        &'a self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
        _host: &'a dyn TdispHostCommandSender,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async move {
            let base_pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;

            // Ensure length_in_bytes is page aligned
            if !length_in_bytes.is_multiple_of(hvdef::HV_PAGE_SIZE as u32) {
                anyhow::bail!("length_in_bytes must be page aligned");
            }

            if length_in_bytes == 0 {
                anyhow::bail!("length_in_bytes must be greater than 0");
            }

            let length_in_pages = length_in_bytes / (hvdef::HV_PAGE_SIZE as u32);

            // Build the full list of PFNs covered by the MMIO range.
            let pfns: Vec<u64> = (0..length_in_pages as u64).map(|i| base_pfn + i).collect();

            tracing::info!(
                base_gpa = format_args!("{:#x}", base_gpa),
                length_in_bytes,
                page_count = pfns.len(),
                first_pfn = format_args!("{:#x}", base_pfn),
                last_pfn = format_args!("{:#x}", base_pfn + length_in_pages as u64 - 1),
                "about to call modify_gpa_visibility(PRIVATE + IMMUTABLE)"
            );

            // Open fresh mshv/mshv_vtl handles on the current VP; these cannot be
            // cached because the VP that created the handle must be the one using
            // it.
            let mshv = Self::open_mshv_hvcall()?;
            let mshv_vtl = Self::open_mshv_vtl()?;

            let guest_device_id = device_id;
            let subrange_base = base_gpa;
            let subrange_page_count = length_in_pages;
            let range_offset = base_offset;
            let validate = true;
            let force_validate = false;

            tracing::info!(
                %guest_device_id,
                %subrange_base,
                %subrange_page_count,
                %range_id,
                %range_offset,
                %validate,
                %force_validate,
                "sending SEV-TIO MMIO validate request"
            );

            // Modify the pages to private before validation
            // New SEV-TIO requirement: pages must be marked immutable in addition to private
            debug_pause_for_breakpoint!(
                "modify_gpa_visibility_and_immutability(PRIVATE, immutable=true) on {} pfn(s) {:#x}..={:#x} (base_gpa {:#x}, {} bytes) for MMIO unblock",
                pfns.len(),
                base_pfn,
                base_pfn + length_in_pages as u64 - 1,
                base_gpa,
                length_in_bytes
            );
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                true,
                &pfns,
            ) {
                Ok(_) => tracing::info!(
                    page_count = pfns.len(),
                    "successfully modified GPA page visibility to private + immutable for MMIO unblock"
                ),
                Err(e) => {
                    tracing::error!(?e, "failed to modify GPA page visibility for MMIO unblock");
                    anyhow::bail!("failed to modify GPA page visibility for MMIO unblock: {e:?}");
                }
            }

            // The pages are now immutable. Arm a guard that clears the immutable bit
            // if we bail before clearing it ourselves on the success path below.
            let immutable_guard = ImmutablePfnGuard::new(&mshv, pfns.clone());

            // Initiate the guest request to mark the MMIO range as validated. The firmware will verify all paging assignments from
            // the host to ensure the range is properly backed by expected guest pages before marking it as validated.
            match self.sev_guest.tio_msg_mmio_validate_req(
                guest_device_id,
                subrange_base,
                subrange_page_count,
                range_offset,
                range_id,
                validate,
                force_validate,
            ) {
                Ok(psp_response) => match psp_response.status {
                    0 => tracing::info!("SEV-TIO MMIO validate request completed successfully"),
                    _ => {
                        tracing::error!(
                            psp_status = psp_response.status,
                            "SEV firmware returned error status for MMIO validate request"
                        );
                        anyhow::bail!(
                            "SEV firmware returned error status for MMIO validate request: {psp_response:?}"
                        );
                    }
                },
                Err(e) => {
                    tracing::error!(?e, "failed to send SEV-TIO MMIO validate request");
                    anyhow::bail!("failed to send SEV-TIO MMIO validate request: {e:?}");
                }
            }

            // Turn off immutability now that the firmware has validated the pages
            debug_pause_for_breakpoint!(
                "modify_gpa_visibility_and_immutability(PRIVATE, immutable=false) on {} pfn(s) {:#x}..={:#x} (base_gpa {:#x}, {} bytes) after the PSP validate call for MMIO unblock",
                pfns.len(),
                base_pfn,
                base_pfn + length_in_pages as u64 - 1,
                base_gpa,
                length_in_bytes
            );
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                false,
                &pfns,
            ) {
                Ok(_) => tracing::info!(
                    page_count = pfns.len(),
                    "successfully modified GPA page immutable=false after PSP call for MMIO unblock"
                ),
                Err(e) => {
                    tracing::error!(
                        ?e,
                        "failed to modify GPA page immutability=false for MMIO unblock"
                    );
                    anyhow::bail!(
                        "failed to modify GPA page immutability=false for MMIO unblock: {e:?}"
                    );
                }
            }

            // Immutability has been cleared on the success path; cancel the rollback.
            immutable_guard.disarm();

            // Page is now in the validated=true and immutable=false state in the RMP. We are free to RMPADJUST
            // now.

            // RMPADJUST the page to be read/write to VTL0 so the guest can access them.
            match mshv_vtl.rmpadjust_pages(
                MemoryRange::from_4k_gpn_range(base_pfn..(base_pfn + (length_in_pages as u64))),
                SevRmpAdjust::new()
                    .with_enable_read(true)
                    .with_enable_write(true)
                    .with_target_vmpl(Self::vtl_to_vmpl(target_vtl))
                    .with_vmsa(false),
                false,
            ) {
                Ok(_) => tracing::info!("successfully rmpadjusted pages for MMIO unblock"),
                Err(e) => {
                    tracing::error!(?e, "failed to rmpadjust pages for MMIO unblock");
                    anyhow::bail!("failed to rmpadjust pages for MMIO unblock: {e:?}");
                }
            }

            Ok(())
        })
    }

    #[tracing::instrument(skip(self), fields(device_id, base_gpa, range_id))]
    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // Take the high order bits of the vtom address (the lower 15 bits are always 0 as vtom is 2MB aligned)
        const SHIFT_2MB: u32 = 15;
        let vtom_high = (self.vtom >> SHIFT_2MB) as u32;

        // Subtract 1 to create the mask for the non-VTOM bit parts of the address
        let vtom = vtom_high - 1;

        let accept_dma = self
            .sev_guest
            .tio_msg_sdte_write_req(device_id, true, vtom, Self::vtl_to_vmpl(target_vtl))
            .context("failed to send SDTE write request")
            .unwrap();
        tracing::info!(msg = format!("SDTE write request response"), response = ?accept_dma);

        match accept_dma.status {
            0 => {
                tracing::info!("SEV-TIO DMA unblock request completed successfully");
            }
            _ => {
                tracing::error!(
                    ?accept_dma,
                    "SEV firmware returned error status for DMA unblock request"
                );
                anyhow::bail!(
                    "SEV firmware returned error status for DMA unblock request: {accept_dma:?}"
                );
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self), fields(device_id, range_id, base_offset, length_in_bytes))]
    fn tdisp_block_mmio<'a>(
        &'a self,
        target_vtl: Vtl,
        device_id: u16,
        base_gpa: u64,
        base_offset: u32,
        length_in_bytes: u32,
        range_id: u16,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + Sync + 'a>> {
        Box::pin(async move {
            let base_pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;

            if !length_in_bytes.is_multiple_of(hvdef::HV_PAGE_SIZE as u32) {
                anyhow::bail!("length_in_bytes must be page aligned");
            }
            if length_in_bytes == 0 {
                anyhow::bail!("length_in_bytes must be greater than 0");
            }

            let length_in_pages = length_in_bytes / (hvdef::HV_PAGE_SIZE as u32);
            let pfns: Vec<u64> = (0..length_in_pages as u64).map(|i| base_pfn + i).collect();

            // Open fresh mshv/mshv_vtl handles on the current VP; these cannot be
            // cached because the VP that created the handle must be the one using
            // it.
            let mshv = Self::open_mshv_hvcall()?;

            // Modify the pages to private and immutable before un-validation
            // New SEV-TIO requirement: pages must be marked immutable in addition to private
            debug_pause_for_breakpoint!(
                "modify_gpa_visibility_and_immutability(PRIVATE, immutable=true) on {} pfn(s) {:#x}..={:#x} (base_gpa {:#x}, {} bytes) for MMIO block",
                pfns.len(),
                base_pfn,
                base_pfn + length_in_pages as u64 - 1,
                base_gpa,
                length_in_bytes
            );
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                true,
                &pfns,
            ) {
                Ok(_) => tracing::info!(
                    page_count = pfns.len(),
                    "successfully modified GPA page visibility to private + immutable for MMIO block"
                ),
                Err(e) => {
                    tracing::error!(?e, "failed to modify GPA page visibility for MMIO block");
                    anyhow::bail!("failed to modify GPA page visibility for MMIO block: {e:?}");
                }
            }

            // The pages are now immutable. Arm a guard that clears the immutable bit
            // if we bail before clearing it ourselves on the success path below.
            let immutable_guard = ImmutablePfnGuard::new(&mshv, pfns.clone());

            // Invalidate the TDI's record of the MMIO range on the PSP.
            let subrange_base = base_gpa;
            let subrange_page_count = length_in_pages;
            match self.sev_guest.tio_msg_mmio_validate_req(
                device_id,
                subrange_base,
                subrange_page_count,
                base_offset,
                range_id,
                /* validate = */ false,
                /* force_validate = */ false,
            ) {
                Ok(psp_response) => match psp_response.status {
                    0 => tracing::info!("SEV-TIO MMIO invalidate request completed successfully"),
                    _ => {
                        tracing::error!(
                            psp_status = psp_response.status,
                            "SEV firmware returned error status for MMIO invalidate request"
                        );
                        anyhow::bail!(
                            "SEV firmware returned error status for MMIO invalidate: {psp_response:?}"
                        );
                    }
                },
                Err(e) => {
                    tracing::error!(?e, "failed to send SEV-TIO MMIO invalidate request");
                    anyhow::bail!("failed to send SEV-TIO MMIO invalidate request: {e:?}");
                }
            }

            // Flip the pages back to shared / host-visible.
            tracing::info!(
                base_gpa = format_args!("{:#x}", base_gpa),
                length_in_bytes,
                page_count = pfns.len(),
                "about to call modify_gpa_visibility(PRIVATE + IMMUTABLE=false)"
            );

            // Remove immutability from the pages before flipping them back to shared
            debug_pause_for_breakpoint!(
                "modify_gpa_visibility_and_immutability(PRIVATE, immutable=false) on {} pfn(s) {:#x}..={:#x} (base_gpa {:#x}, {} bytes) for MMIO block",
                pfns.len(),
                base_pfn,
                base_pfn + length_in_pages as u64 - 1,
                base_gpa,
                length_in_bytes
            );
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                false,
                &pfns,
            ) {
                Ok(_) => tracing::info!(
                    page_count = pfns.len(),
                    "successfully flipped GPA pages back to shared for MMIO block"
                ),
                Err(e) => {
                    tracing::error!(?e, "failed to modify GPA page visibility for MMIO block");
                    anyhow::bail!("failed to modify GPA page visibility for MMIO block: {e:?}");
                }
            }

            // Immutability has been cleared on the success path; cancel the rollback.
            immutable_guard.disarm();

            // Flip the pages back to shared / host-visible.
            tracing::info!(
                base_gpa = format_args!("{:#x}", base_gpa),
                length_in_bytes,
                page_count = pfns.len(),
                "about to call modify_gpa_visibility(SHARED)"
            );

            debug_pause_for_breakpoint!(
                "modify_gpa_visibility(SHARED) on {} pfn(s) {:#x}..={:#x} (base_gpa {:#x}, {} bytes) for MMIO block",
                pfns.len(),
                base_pfn,
                base_pfn + length_in_pages as u64 - 1,
                base_gpa,
                length_in_bytes
            );
            match mshv.modify_gpa_visibility(HostVisibilityType::SHARED, &pfns) {
                Ok(_) => tracing::info!(
                    page_count = pfns.len(),
                    "successfully flipped GPA pages back to shared for MMIO block"
                ),
                Err(e) => {
                    tracing::error!(?e, "failed to modify GPA page visibility for MMIO block");
                    anyhow::bail!("failed to modify GPA page visibility for MMIO block: {e:?}");
                }
            }

            Ok(())
        })
    }

    #[tracing::instrument(skip(self), fields(device_id))]
    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // Write a zero-valued SDTE so the IOMMU blocks DMA from this device.
        let block_dma = self
            .sev_guest
            .tio_msg_sdte_write_req(device_id, false, 0, Self::vtl_to_vmpl(target_vtl))
            .context("failed to send SDTE block request")
            .unwrap();
        tracing::info!(response = ?block_dma, "SDTE block request response");

        match block_dma.status {
            0 => {
                tracing::info!("SEV-TIO DMA block request completed successfully");
                Ok(())
            }
            _ => {
                tracing::error!(
                    ?block_dma,
                    "SEV firmware returned error status for DMA block request"
                );
                anyhow::bail!(
                    "SEV firmware returned error status for DMA block request: {block_dma:?}"
                )
            }
        }
    }
}
