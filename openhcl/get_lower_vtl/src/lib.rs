// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This is an implementation of [`VtlMemoryProtection`] that uses the
//! [`MshvHvcall`] type. This is only to be used for the GET, as we cannot use
//! the normal partition implementation in OpenHCL due to ordering requirements
//! for struct initialization.

use anyhow::Context;
use anyhow::Result;
use hvdef::hypercall::HvInputVtl;
use inspect::Inspect;
use memory_range::MemoryRange;
use std::sync::Arc;
use virt::VtlMemoryProtection;

#[derive(Inspect)]
pub struct GetLowerVtl {
    #[inspect(skip)]
    mshv_hvcall: hcl::ioctl::MshvHvcall,
}

impl GetLowerVtl {
    pub fn new() -> Result<Arc<Self>> {
        let mshv_hvcall = hcl::ioctl::MshvHvcall::new().context("failed to open mshv_hvcall")?;
        mshv_hvcall.set_allowed_hypercalls(&[hvdef::HypercallCode::HvCallModifyVtlProtectionMask]);
        Ok(Arc::new(Self { mshv_hvcall }))
    }
}

impl VtlMemoryProtection for GetLowerVtl {
    fn modify_vtl_page_setting(&self, pfn: u64, flags: hvdef::HvMapGpaFlags) -> Result<()> {
        self.mshv_hvcall
            .modify_vtl_protection_mask(
                MemoryRange::from_4k_gpn_range(pfn..pfn + 1),
                flags,
                HvInputVtl::CURRENT_VTL,
            )
            .context("failed to modify VTL page permissions")
    }
}
