// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]

//! Implements a VtlMemoryProtection guard that can be used to temporarily allow
//! access to pages that were previously protected.

#![warn(missing_docs)]
#[cfg(feature = "vfio")]
mod device_dma;

#[cfg(feature = "vfio")]
pub use device_dma::LowerVtlDmaBuffer;

use anyhow::Context;
use anyhow::Result;
use inspect::Inspect;
use std::sync::Arc;
#[cfg(all(feature = "vfio", target_os = "linux"))]
use user_driver::memory::MemoryBlock;
#[cfg(all(feature = "vfio", target_os = "linux"))]
use user_driver::vfio::VfioDmaBuffer;
use virt::VtlMemoryProtection;

/// A guard that will restore [`hvdef::HV_MAP_GPA_PERMISSIONS_NONE`] permissions
/// on the pages when dropped.
#[derive(Inspect)]
struct PagesAccessibleToLowerVtl {
    #[inspect(skip)]
    vtl_protect: Arc<dyn VtlMemoryProtection + Send + Sync>,
    #[inspect(with = "|x| inspect::iter_by_index(x).map_value(inspect::AsHex)")]
    pages: Vec<u64>,
}

impl PagesAccessibleToLowerVtl {
    /// Creates a new guard that will lower the VTL permissions of the pages
    /// while the returned guard is held.
    fn new_from_pages(
        vtl_protect: Arc<dyn VtlMemoryProtection + Send + Sync>,
        pages: &[u64],
    ) -> Result<Self> {
        for pfn in pages {
            vtl_protect
                .modify_vtl_page_setting(*pfn, hvdef::HV_MAP_GPA_PERMISSIONS_ALL)
                .context("failed to update VTL protections on page")?;
        }
        Ok(Self {
            vtl_protect,
            pages: pages.to_vec(),
        })
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
            // The inability to rollback any pages is fatal. We cannot leave the
            // pages in the state where the correct VTL protections are not
            // applied, because that would compromise the security of the
            // platform.
            panic!(
                "failed to reset page protections {}",
                err.as_ref() as &dyn std::error::Error
            );
        }
    }
}

/// A [`VfioDmaBuffer`] wrapper that will lower the VTL permissions of the page
/// on the allocated memory block.
#[cfg(all(feature = "vfio", target_os = "linux"))]
pub struct LowerVtlMemorySpawner<T: VfioDmaBuffer> {
    spawner: T,
    vtl_protect: Arc<dyn VtlMemoryProtection + Send + Sync>,
}

#[cfg(all(feature = "vfio", target_os = "linux"))]
impl<T: VfioDmaBuffer> LowerVtlMemorySpawner<T> {
    /// Create a new wrapped [`VfioDmaBuffer`] spawner that will lower the VTL
    /// permissions of the returned [`MemoryBlock`].
    pub fn new(spawner: T, vtl_protect: Arc<dyn VtlMemoryProtection + Send + Sync>) -> Self {
        Self {
            spawner,
            vtl_protect,
        }
    }
}

#[cfg(all(feature = "vfio", target_os = "linux"))]
impl<T: VfioDmaBuffer> VfioDmaBuffer for LowerVtlMemorySpawner<T> {
    fn create_dma_buffer(&self, len: usize) -> Result<MemoryBlock> {
        let mem = self.spawner.create_dma_buffer(len)?;
        let vtl_guard =
            PagesAccessibleToLowerVtl::new_from_pages(self.vtl_protect.clone(), mem.pfns())
                .context("failed to lower VTL permissions on memory block")?;

        Ok(MemoryBlock::new(LowerVtlDmaBuffer {
            block: mem,
            _vtl_guard: vtl_guard,
        }))
    }
}
