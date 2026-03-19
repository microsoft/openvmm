// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides an implementation of the SEV-TIO resource validation interface for
//! TDISP devices. This is used by OpenHCL devices that are exposed to SEV guests and need to
//! communicate with the SEV firmware to unblock device resources after attestation.

use crate::TdispResourceValidationInterface;
use anyhow::Context;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvHvcall;
use hcl::ioctl::MshvVtl;
use hvdef::Vtl;
use hvdef::hypercall::HostVisibilityType;
use memory_range::MemoryRange;
use sev_guest_device::SevGuestDevice;
use x86defs::snp::SevRmpAdjust;

/// AMD SEV-TIO implementation of [`TdispResourceValidationInterface`].
///
/// After a device has been attested and placed in the Run state, this struct
/// communicates with the SEV firmware via `/dev/sev-guest` and issues
/// hypercalls to make device resources (MMIO, DMA) accessible to the guest.
pub struct TdispSevTioResourceValidator {
    sev_guest: SevGuestDevice,
    mshv: MshvHvcall,
    mshv_vtl: MshvVtl,
    vtom: u64,
}

impl TdispSevTioResourceValidator {
    /// Open handles to the `/dev/sev-guest` device and the hypervisor call
    /// interface required for SEV-TIO operations.
    ///
    /// * `vtom` - The address mask with the VTOM bit set to signify where VTOM
    ///   addresses start in the CVM.
    pub fn new(vtom: u64) -> anyhow::Result<Self> {
        let sev_guest = SevGuestDevice::open()
            .context("failed to open /dev/sev-guest")
            .unwrap();

        let mshv = MshvHvcall::new().context("failed to open mshv_hvcall device")?;
        mshv.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallModifySparseGpaPageHostVisibility,
        ]);

        let mshv_vtl_changer = Mshv::new().context("failed to create mshv").unwrap();
        let mshv_vtl = mshv_vtl_changer
            .create_vtl()
            .context("failed to create mshv vtl")
            .unwrap();

        Ok(Self {
            sev_guest,
            mshv,
            mshv_vtl,
            vtom,
        })
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
    #[tracing::instrument(skip(self), fields(device_id, range_id, base_offset, length_in_bytes))]
    fn tdisp_unblock_mmio(
        &self,
        target_vtl: Vtl,
        device_id: u64,
        base_gpa: u64,
        base_offset: u64,
        length_in_bytes: u64,
        range_id: u64,
    ) -> anyhow::Result<()> {
        let pfn = base_gpa >> hvdef::HV_PAGE_SHIFT;

        // Ensure length_in_bytes is page aligned
        if !length_in_bytes.is_multiple_of(hvdef::HV_PAGE_SIZE) {
            anyhow::bail!("length_in_bytes must be page aligned");
        }

        if length_in_bytes == 0 {
            anyhow::bail!("length_in_bytes must be greater than 0");
        }

        // Modify the pages to private before validation
        match self
            .mshv
            .modify_gpa_visibility(HostVisibilityType::PRIVATE, &[pfn])
        {
            Ok(_) => tracing::info!(
                "successfully modified GPA page visibility to private for MMIO unblock"
            ),
            Err(e) => {
                tracing::error!(?e, "failed to modify GPA page visibility for MMIO unblock");
                anyhow::bail!("failed to modify GPA page visibility for MMIO unblock: {e:?}");
            }
        }

        let length_in_pages = length_in_bytes / hvdef::HV_PAGE_SIZE;
        let guest_device_id = u16::try_from(device_id).context("device_id must fit within u16")?;
        let subrange_base = base_gpa;
        let subrange_page_count =
            u32::try_from(length_in_pages).context("length_in_pages must fit within u32")?;
        let range_id = u16::try_from(range_id).context("range_id must fit within u16")?;
        let range_offset = u32::try_from(base_offset).context("base_offset must fit within u32")?;
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

        // Finally, rmpadjust the pages to be read/write to VTL0 so the guest can access them.
        match self.mshv_vtl.rmpadjust_pages(
            MemoryRange::from_4k_gpn_range(pfn..pfn + length_in_pages),
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
    }

    #[tracing::instrument(skip(self), fields(device_id, base_gpa, range_id))]
    fn tdisp_unblock_dma(&self, target_vtl: Vtl, device_id: u64) -> anyhow::Result<()> {
        // Take the high order bits of the vtom address (the lower 15 bits are always 0 as vtom is 2MB aligned)
        const SHIFT_2MB: u32 = 15;
        let vtom_high = (self.vtom >> SHIFT_2MB) as u32;

        // Subtract 1 to create the mask for the non-VTOM bit parts of the address
        let vtom = vtom_high - 1;

        // TDISP TODO: Validate that this calculation above is correct
        if vtom != 0x7fffffff {
            anyhow::bail!("unexpected VTOM value: {vtom:#x}, expected 0x7fffffff");
        }

        let accept_dma = self
            .sev_guest
            .tio_msg_sdte_write_req(
                u16::try_from(device_id).context("device_id must fit within u16")?,
                vtom,
                Self::vtl_to_vmpl(target_vtl),
            )
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
}
