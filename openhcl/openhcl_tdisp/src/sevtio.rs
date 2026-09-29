// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides an implementation of the SEV-TIO resource validation interface for
//! TDISP devices. This is used by OpenHCL devices that are exposed to SEV guests and need to
//! communicate with the SEV firmware to unblock device resources after attestation.

use crate::TdispResourceValidationInterface;
use crate::TdispTdiState;
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
use tdisp::devicereport::TdiReportStruct;
use x86defs::snp::SevRmpAdjust;

/// AMD SEV-TIO implementation of [`TdispResourceValidationInterface`].
///
/// Communicates with the SEV firmware via `/dev/sev-guest` to manage TDISP
/// device resources for SEV guests.
pub struct TdispSevTioResourceValidator {
    sev_guest: SevGuestDevice,
    vtom: u64,
}

impl TdispSevTioResourceValidator {
    /// Open a handle to the `/dev/sev-guest` device required for SEV-TIO
    /// operations.
    ///
    /// Note: the `mshv` and `mshv_vtl` handles must be recreated for each
    /// request to ensure they are created on the correct VP.
    ///
    /// * `vtom` - The address mask with the VTOM bit set to signify where VTOM
    ///   addresses start in the CVM.
    pub fn new(vtom: u64) -> anyhow::Result<Self> {
        let sev_guest = SevGuestDevice::open().context("failed to open /dev/sev-guest")?;

        Ok(Self { sev_guest, vtom })
    }

    /// Open `MshvHvcall` handle for a single request.
    fn open_mshv_hvcall() -> anyhow::Result<MshvHvcall> {
        let mshv = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        mshv.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallModifySparseGpaPageHostVisibility,
        ]);
        Ok(mshv)
    }

    /// Open `MshvVtl` handle for a single request.
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
    fn on_pre_bind(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // SEV-TIO has nothing to do before the bind.
        tracelimit::info_ratelimited!(?target_vtl, device_id, "SEV-TIO on_pre_bind: no-op");
        Ok(())
    }

    fn on_pre_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // See `on_pre_bind`.
        tracelimit::info_ratelimited!(?target_vtl, device_id, "SEV-TIO on_pre_start: no-op");
        Ok(())
    }

    fn on_post_start(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // See `on_pre_bind`.
        tracelimit::info_ratelimited!(?target_vtl, device_id, "SEV-TIO on_post_start: no-op");
        Ok(())
    }

    fn get_tsm_tdi_state(
        &self,
        target_vtl: Vtl,
        device_id: u16,
    ) -> anyhow::Result<Option<TdispTdiState>> {
        // TDISP TODO: SEV module does not properly support this functionality
        // yet. Pending support from the SEV firmware developers.
        tracelimit::info_ratelimited!(
            ?target_vtl,
            device_id,
            "SEV-TIO get_tsm_tdi_state: not implemented"
        );
        Ok(None)
    }

    fn tdisp_set_tdi_report(&self, device_id: u16, _report: &TdiReportStruct) {
        // SEV-TIO addresses MMIO ranges by range id, so it has no use for this
        // report.
        tracelimit::info_ratelimited!(device_id, "SEV-TIO tdisp_set_tdi_report: no-op");
    }

    fn tdisp_clear_tdi_report(&self, device_id: u16) {
        // See `tdisp_set_tdi_report`.
        tracelimit::info_ratelimited!(device_id, "SEV-TIO tdisp_clear_tdi_report: no-op");
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
            let base_pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;

            // Ensure length_in_bytes is page aligned
            if !length_in_bytes.is_multiple_of(hvdef::HV_PAGE_SIZE) {
                anyhow::bail!("length_in_bytes must be page aligned");
            }

            if length_in_bytes == 0 {
                anyhow::bail!("length_in_bytes must be greater than 0");
            }

            let length_in_pages = length_in_bytes / hvdef::HV_PAGE_SIZE;

            // Build the full list of PFNs covered by the MMIO range.
            let pfns: Vec<u64> = (0..length_in_pages).map(|i| base_pfn + i).collect();

            tracelimit::info_ratelimited!(
                base_gpa = format_args!("{:#x}", base_gpa),
                length_in_bytes,
                page_count = pfns.len(),
                first_pfn = format_args!("{:#x}", base_pfn),
                last_pfn = format_args!("{:#x}", base_pfn + length_in_pages - 1),
                "about to call modify_gpa_visibility(PRIVATE + IMMUTABLE)"
            );

            let mshv = Self::open_mshv_hvcall()?;
            let mshv_vtl = Self::open_mshv_vtl()?;

            let guest_device_id = device_id;
            let subrange_base = base_gpa;
            let subrange_page_count =
                u32::try_from(length_in_pages).context("MMIO range is more than u32::MAX pages")?;
            let range_offset = base_offset;
            let validate = true;
            let force_validate = false;

            tracelimit::info_ratelimited!(
                %guest_device_id,
                %subrange_base,
                %subrange_page_count,
                %range_id,
                %range_offset,
                %validate,
                %force_validate,
                "sending SEV-TIO MMIO validate request"
            );

            // Modify the pages to private before validation.
            // SEV-TIO requires pages be marked immutable in addition to private.
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                true,
                &pfns,
            ) {
                Ok(_) => tracelimit::info_ratelimited!(
                    page_count = pfns.len(),
                    "successfully modified GPA page visibility to private + immutable for MMIO unblock"
                ),
                Err((e, processed)) => {
                    // A partial failure leaves some pages private and immutable
                    // with nothing recording which ones, so there is no state
                    // left that the guest can safely run against.
                    panic!(
                        "failed to modify GPA page visibility for MMIO unblock, \
                         {processed} of {} pages left private and immutable: {e:?}",
                        pfns.len()
                    );
                }
            }

            // Initiate the guest request to mark the MMIO range as validated.
            // The firmware will verify all paging assignments from the host to
            // ensure the range is properly backed by expected guest pages
            // before marking it as validated.
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
                    0 => tracelimit::info_ratelimited!(
                        "SEV-TIO MMIO validate request completed successfully"
                    ),
                    _ => {
                        tracing::error!(
                            psp_status = psp_response.status,
                            "SEV firmware returned error status for MMIO validate request"
                        );
                        panic!(
                            "SEV firmware returned error status for MMIO validate request, \
                             {} pages left immutable: {psp_response:?}",
                            pfns.len()
                        );
                    }
                },
                Err(e) => {
                    tracing::error!(?e, "failed to send SEV-TIO MMIO validate request");
                    panic!(
                        "failed to send SEV-TIO MMIO validate request, {} pages left immutable: {e:?}",
                        pfns.len()
                    );
                }
            }

            // Turn off immutability now that the firmware has validated the pages
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                false,
                &pfns,
            ) {
                Ok(_) => tracelimit::info_ratelimited!(
                    page_count = pfns.len(),
                    "successfully modified GPA page immutable=false after PSP call for MMIO unblock"
                ),
                Err((e, processed)) => {
                    tracing::error!(
                        ?e,
                        "failed to modify GPA page immutability=false for MMIO unblock"
                    );
                    panic!(
                        "failed to clear GPA page immutability for MMIO unblock, {processed} of \
                         {} pages cleared, {} left immutable with the range already validated: {e:?}",
                        pfns.len(),
                        pfns.len() - processed
                    );
                }
            }

            // RMPADJUST the page to be read/write to VTL0 so the guest can access them.
            match mshv_vtl.rmpadjust_pages(
                MemoryRange::from_4k_gpn_range(base_pfn..(base_pfn + length_in_pages)),
                SevRmpAdjust::new()
                    .with_enable_read(true)
                    .with_enable_write(true)
                    .with_target_vmpl(Self::vtl_to_vmpl(target_vtl))
                    .with_vmsa(false),
                false,
            ) {
                Ok(_) => {
                    tracelimit::info_ratelimited!("successfully rmpadjusted pages for MMIO unblock")
                }
                Err(e) => {
                    tracing::error!(?e, "failed to rmpadjust pages for MMIO unblock");
                    panic!(
                        "failed to rmpadjust pages for MMIO unblock, {} pages left private \
                         with the range already validated: {e:?}",
                        pfns.len()
                    );
                }
            }

            Ok(())
        })
    }

    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // The SDTE address field holds VTOM address bits [46:16]. Shift the mask
        // down to bit 16 and subtract one, turning the VTOM bit into a mask of
        // every address bit below it, so a bit-47 VTOM encodes as 0x7fff_ffff.
        // The subtraction stays in u64 so the intermediate cannot truncate.
        const VTOM_SHIFT: u32 = 16;
        const SDTE_VTOM_BITS: u32 = 31;

        let mask = (self.vtom >> VTOM_SHIFT).checked_sub(1).with_context(|| {
            format!(
                "VTOM {:#x} is too small to form an SDTE address mask",
                self.vtom
            )
        })?;

        anyhow::ensure!(
            mask >> SDTE_VTOM_BITS == 0,
            "VTOM {:#x} does not fit the {SDTE_VTOM_BITS}-bit SDTE address field",
            self.vtom
        );

        let vtom = mask as u32;

        let accept_dma = self
            .sev_guest
            .tio_msg_sdte_write_req(device_id, true, vtom, Self::vtl_to_vmpl(target_vtl))
            .context("failed to send SDTE write request")?;
        tracelimit::info_ratelimited!(response = ?accept_dma, "SDTE write request response");

        match accept_dma.status {
            0 => {
                tracelimit::info_ratelimited!("SEV-TIO DMA unblock request completed successfully");
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
            let base_pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;

            if !length_in_bytes.is_multiple_of(hvdef::HV_PAGE_SIZE) {
                anyhow::bail!("length_in_bytes must be page aligned");
            }
            if length_in_bytes == 0 {
                anyhow::bail!("length_in_bytes must be greater than 0");
            }

            let length_in_pages = length_in_bytes / hvdef::HV_PAGE_SIZE;
            let pfns: Vec<u64> = (0..length_in_pages).map(|i| base_pfn + i).collect();
            let mshv = Self::open_mshv_hvcall()?;
            let mshv_vtl = Self::open_mshv_vtl()?;

            // Convert the page count before touching page state, so an
            // unrepresentable range is rejected while it is still recoverable.
            let subrange_base = base_gpa;
            let subrange_page_count =
                u32::try_from(length_in_pages).context("MMIO range is more than u32::MAX pages")?;

            // Take the guest's access away before anything else, so it cannot
            // reach the range while the firmware is invalidating it.
            match mshv_vtl.rmpadjust_pages(
                MemoryRange::from_4k_gpn_range(base_pfn..(base_pfn + length_in_pages)),
                SevRmpAdjust::new()
                    .with_enable_read(false)
                    .with_enable_write(false)
                    .with_target_vmpl(Self::vtl_to_vmpl(target_vtl))
                    .with_vmsa(false),
                false,
            ) {
                Ok(_) => tracelimit::info_ratelimited!(
                    page_count = pfns.len(),
                    "successfully revoked guest RMP permissions for MMIO block"
                ),
                Err(e) => {
                    tracing::error!(?e, "failed to revoke guest RMP permissions for MMIO block");
                    panic!(
                        "failed to revoke guest RMP permissions for MMIO block, the guest may \
                         retain access to {} pages: {e:?}",
                        pfns.len()
                    );
                }
            }

            // Modify the pages to private and immutable before de-validation.
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                true,
                &pfns,
            ) {
                Ok(_) => tracelimit::info_ratelimited!(
                    page_count = pfns.len(),
                    "successfully modified GPA page visibility to private + immutable for MMIO block"
                ),
                Err((e, processed)) => {
                    // As on the unblock path, a partial failure leaves pages
                    // immutable with nothing recording which ones.
                    panic!(
                        "failed to modify GPA page visibility for MMIO block, \
                         {processed} of {} pages left immutable: {e:?}",
                        pfns.len()
                    );
                }
            }

            // Invalidate the TDI's record of the MMIO range on the PSP.
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
                    0 => tracelimit::info_ratelimited!(
                        "SEV-TIO MMIO invalidate request completed successfully"
                    ),
                    _ => {
                        tracing::error!(
                            psp_status = psp_response.status,
                            "SEV firmware returned error status for MMIO invalidate request"
                        );
                        panic!(
                            "SEV firmware returned error status for MMIO invalidate, \
                             {} pages left immutable: {psp_response:?}",
                            pfns.len()
                        );
                    }
                },
                Err(e) => {
                    tracing::error!(?e, "failed to send SEV-TIO MMIO invalidate request");
                    panic!(
                        "failed to send SEV-TIO MMIO invalidate request, {} pages left immutable: {e:?}",
                        pfns.len()
                    );
                }
            }

            tracelimit::info_ratelimited!(
                base_gpa = format_args!("{:#x}", base_gpa),
                length_in_bytes,
                page_count = pfns.len(),
                "about to call modify_gpa_visibility(PRIVATE + IMMUTABLE=false)"
            );

            // Remove immutability from the pages before flipping them back to shared
            match mshv.modify_gpa_visibility_and_immutability(
                HostVisibilityType::PRIVATE,
                false,
                &pfns,
            ) {
                Ok(_) => tracelimit::info_ratelimited!(
                    page_count = pfns.len(),
                    "successfully flipped GPA pages back to shared for MMIO block"
                ),
                Err((e, processed)) => {
                    panic!(
                        "failed to clear GPA page immutability for MMIO block, {processed} of \
                         {} pages cleared, {} left immutable: {e:?}",
                        pfns.len(),
                        pfns.len() - processed
                    );
                }
            }

            // Flip the pages back to shared / host-visible.
            tracelimit::info_ratelimited!(
                base_gpa = format_args!("{:#x}", base_gpa),
                length_in_bytes,
                page_count = pfns.len(),
                "about to call modify_gpa_visibility(SHARED)"
            );

            match mshv.modify_gpa_visibility(HostVisibilityType::SHARED, &pfns) {
                Ok(_) => tracelimit::info_ratelimited!(
                    page_count = pfns.len(),
                    "successfully flipped GPA pages back to shared for MMIO block"
                ),
                Err((e, processed)) => {
                    panic!(
                        "failed to flip GPA pages back to shared for MMIO block, {processed} of \
                         {} pages flipped, {} left private: {e:?}",
                        pfns.len(),
                        pfns.len() - processed
                    );
                }
            }

            Ok(())
        })
    }

    fn tdisp_block_dma(&self, target_vtl: Vtl, device_id: u16) -> anyhow::Result<()> {
        // Write a zero-valued SDTE so the IOMMU blocks DMA from this device.
        let block_dma = self
            .sev_guest
            .tio_msg_sdte_write_req(device_id, false, 0, Self::vtl_to_vmpl(target_vtl))
            .context("failed to send SDTE block request")?;
        tracelimit::info_ratelimited!(response = ?block_dma, "SDTE block request response");

        match block_dma.status {
            0 => {
                tracelimit::info_ratelimited!("SEV-TIO DMA block request completed successfully");
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
