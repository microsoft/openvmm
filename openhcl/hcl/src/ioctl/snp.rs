// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Backing for SNP partitions.

use super::Hcl;
use super::HclVp;
use super::MshvVtl;
use super::NoRunner;
use super::ProcessorRunner;
use super::hcl_pvalidate_pages;
use super::hcl_rmpadjust_pages;
use super::hcl_rmpquery_pages;
use super::mshv_pvalidate;
use super::mshv_rmpadjust;
use super::mshv_rmpquery;
use crate::GuestVtl;
use crate::vmsa::VmsaWrapper;
use hv1_structs::VtlArray;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvRegisterName;
use hvdef::HvRegisterValue;
use memory_range::MemoryRange;
use sidecar_client::SidecarVp;
use std::cell::UnsafeCell;
use std::os::fd::AsRawFd;
use thiserror::Error;
use x86defs::snp::SevRmpAdjust;
use x86defs::snp::SevVmsa;

/// Runner backing for SNP partitions.
pub struct Snp<'a> {
    vmsa: VtlArray<&'a UnsafeCell<SevVmsa>, 2>,
}

/// Error returned by failing SNP operations.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum SnpError {
    #[error("operating system error")]
    Os(#[source] nix::Error),
    #[error("isa error {0:?}")]
    Isa(u32),
}

/// Error returned by failing SNP page operations.
#[derive(Debug, Error)]
#[expect(missing_docs)]
pub enum SnpPageError {
    #[error("pvalidate failed")]
    Pvalidate(#[source] SnpError),
    #[error("rmpadjust failed")]
    Rmpadjust(#[source] SnpError),
    #[error("rmpquery failed")]
    Rmpquery(#[source] SnpError),
}

impl MshvVtl {
    /// Execute the pvalidate instruction on the specified memory range.
    ///
    /// The range must not be mapped in the kernel as RAM.
    pub fn pvalidate_pages(
        &self,
        range: MemoryRange,
        validate: bool,
        terminate_on_failure: bool,
    ) -> Result<(), SnpPageError> {
        tracing::debug!(%range, validate, terminate_on_failure, "pvalidate");
        // SAFETY: TODO SNP FUTURE: we are passing parameters as the kernel requires.
        // For defense in depth it could be useful to prevent usermode from changing
        // visibility of a VTL2 kernel page in the kernel.
        let ret = unsafe {
            hcl_pvalidate_pages(
                self.file.as_raw_fd(),
                &mshv_pvalidate {
                    start_pfn: range.start() / HV_PAGE_SIZE,
                    page_count: (range.end() - range.start()) / HV_PAGE_SIZE,
                    validate: validate as u8,
                    terminate_on_failure: terminate_on_failure as u8,
                    ram: 0,
                    padding: [0; 1],
                },
            )
            .map_err(SnpError::Os)
            .map_err(SnpPageError::Pvalidate)?
        };

        if ret != 0 {
            return Err(SnpPageError::Pvalidate(SnpError::Isa(ret as u32)));
        }

        Ok(())
    }

    /// Execute the rmpadjust instruction on the specified memory range.
    ///
    /// The range must not be mapped in the kernel as RAM.
    ///
    /// Note: The VMSA pages for VTL0 and VTL1 are converted to a VMSA page
    /// in the RMP by the kernel, in mshv_configure_vmsa_page. The VTL2 VMSA
    /// page is converted via SNP_LAUNCH_UPDATE.
    pub fn rmpadjust_pages(
        &self,
        range: MemoryRange,
        value: SevRmpAdjust,
        terminate_on_failure: bool,
    ) -> Result<(), SnpPageError> {
        // SAFETY: TODO SNP FUTURE: For defense in depth it could be useful to prevent
        // usermode from changing permissions of a VTL2 kernel page in the kernel.
        let ret = unsafe {
            hcl_rmpadjust_pages(
                self.file.as_raw_fd(),
                &mshv_rmpadjust {
                    start_pfn: range.start() / HV_PAGE_SIZE,
                    page_count: (range.end() - range.start()) / HV_PAGE_SIZE,
                    value: value.into(),
                    terminate_on_failure: terminate_on_failure as u8,
                    ram: 0,
                    padding: Default::default(),
                },
            )
            .map_err(SnpError::Os)
            .map_err(SnpPageError::Rmpadjust)?
        };

        if ret != 0 {
            return Err(SnpPageError::Rmpadjust(SnpError::Isa(ret as u32)));
        }

        Ok(())
    }

    /// Gets the current vtl permissions for a page.
    /// Note: only supported on Genoa+
    pub fn rmpquery_page(&self, gpa: u64, vtl: GuestVtl) -> Result<SevRmpAdjust, SnpPageError> {
        let page_count = 1u64;
        let mut flags = [u64::from(SevRmpAdjust::new().with_target_vmpl(match vtl {
            GuestVtl::Vtl0 => 2,
            GuestVtl::Vtl1 => 1,
        })); 1];

        let mut page_size = [0; 1];
        let mut pages_processed = 0u64;

        debug_assert!(flags.len() == page_count as usize);
        debug_assert!(page_size.len() == page_count as usize);

        let query = mshv_rmpquery {
            start_pfn: gpa / HV_PAGE_SIZE,
            page_count,
            terminate_on_failure: 0,
            ram: 0,
            padding: Default::default(),
            flags: flags.as_mut_ptr(),
            page_size: page_size.as_mut_ptr(),
            pages_processed: &mut pages_processed,
        };

        // SAFETY: the input query is the correct type for this ioctl
        unsafe {
            hcl_rmpquery_pages(self.file.as_raw_fd(), &query)
                .map_err(SnpError::Os)
                .map_err(SnpPageError::Rmpquery)?;
        }

        assert!(pages_processed <= page_count);

        Ok(SevRmpAdjust::from(flags[0]))
    }
}

impl<'a> super::private::BackingPrivate<'a> for Snp<'a> {
    fn new(vp: &'a HclVp, sidecar: Option<&SidecarVp<'_>>, _hcl: &Hcl) -> Result<Self, NoRunner> {
        assert!(sidecar.is_none());
        let super::BackingState::Snp { vmsa } = &vp.backing else {
            return Err(NoRunner::MismatchedIsolation);
        };

        Ok(Self {
            vmsa: vmsa.each_ref().map(|mp| mp.as_ref()),
        })
    }

    fn try_set_reg(
        _runner: &mut ProcessorRunner<'a, Self>,
        _vtl: GuestVtl,
        _name: HvRegisterName,
        _value: HvRegisterValue,
    ) -> bool {
        false
    }

    fn must_flush_regs_on(_runner: &ProcessorRunner<'a, Self>, _name: HvRegisterName) -> bool {
        false
    }

    fn try_get_reg(
        _runner: &ProcessorRunner<'a, Self>,
        _vtl: GuestVtl,
        _name: HvRegisterName,
    ) -> Option<HvRegisterValue> {
        None
    }

    fn flush_register_page(_runner: &mut ProcessorRunner<'a, Self>) {}
}

impl<'a> ProcessorRunner<'a, Snp<'a>> {
    /// Gets a reference to the VMSA and backing state of a VTL
    pub fn vmsa(&self, vtl: GuestVtl) -> VmsaWrapper<'_, &SevVmsa> {
        // SAFETY: the VMSA will not be concurrently accessed by the processor
        // while this VP is in VTL2.
        let vmsa = unsafe { &*self.state.vmsa[vtl].get() };

        VmsaWrapper::new(vmsa, &self.hcl.snp_register_bitmap)
    }

    /// Gets a mutable reference to the VMSA and backing state of a VTL.
    pub fn vmsa_mut(&mut self, vtl: GuestVtl) -> VmsaWrapper<'_, &mut SevVmsa> {
        // SAFETY: the VMSA will not be concurrently accessed by the processor
        // while this VP is in VTL2.
        let vmsa = unsafe { &mut *self.state.vmsa[vtl].get() };

        VmsaWrapper::new(vmsa, &self.hcl.snp_register_bitmap)
    }

    /// Returns the VMSAs for [VTL0, VTL1].
    pub fn vmsas_mut(&mut self) -> [VmsaWrapper<'_, &mut SevVmsa>; 2] {
        self.state
            .vmsa
            .each_mut()
            .map(|vmsa| {
                // SAFETY: the VMSA will not be concurrently accessed by the processor
                // while this VP is in VTL2.
                let vmsa = unsafe { &mut *vmsa.get() };

                VmsaWrapper::new(vmsa, &self.hcl.snp_register_bitmap)
            })
            .into_inner()
    }
}
