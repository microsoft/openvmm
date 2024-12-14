// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements a VtlMemoryProtection guard that can be used to temporarily allow
//! access to pages that were previously protected.

#![forbid(unsafe_code)]

use anyhow::Context;
use anyhow::Result;
use inspect::Inspect;
use std::sync::Arc;
use user_driver::memory::MemoryBlock;
use virt::VtlMemoryProtection;

#[derive(Inspect)]
pub struct PagesAccessibleToLowerVtl {
    #[inspect(skip)]
    vtl_protect: Arc<dyn VtlMemoryProtection + Send + Sync>,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    pages: Vec<u64>,
}

impl PagesAccessibleToLowerVtl {
    pub fn new_from_memory_block(
        vtl_protect: Arc<dyn VtlMemoryProtection + Send + Sync>,
        memory: &MemoryBlock,
    ) -> Result<Self> {
        let pages = Vec::with_capacity(memory.pfns().len());
        let mut this = Self { vtl_protect, pages };
        for pfn in memory.pfns() {
            this.vtl_protect
                .modify_vtl_page_setting(*pfn, hvdef::HV_MAP_GPA_PERMISSIONS_ALL)
                .context("failed to update VTL protections on page")?;
            this.pages.push(*pfn);
        }
        Ok(this)
    }

    pub fn pfns(&self) -> &[u64] {
        &self.pages
    }
}

impl Drop for PagesAccessibleToLowerVtl {
    fn drop(&mut self) {
        if let Err(err) = self
            .pages
            .iter()
            .map(|pfn| {
                self.vtl_protect
                    .modify_vtl_page_setting(*pfn, hvdef::HV_MAP_GPA_PERMISSIONS_NONE)
                    .context("failed to update VTL protections on page")
            })
            .collect::<Result<Vec<_>>>()
        {
            panic!(
                "Failed to reset page protections {}",
                err.as_ref() as &dyn std::error::Error
            );
        }
    }
}
