// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! iommufd noiommu IOAS handle for cdev-based VFIO devices.
//!
//! With the modern cdev + iommufd interface a VFIO device is attached to an
//! IOAS at bind time. For a noiommu device the IOAS only needs to exist to
//! satisfy that bind/attach: the device performs untranslated DMA directly to
//! physical addresses, so individual DMA buffers do not need to be mapped into
//! the IOAS. The device is programmed with the page pool's physical page
//! numbers directly, exactly as in the legacy VFIO noiommu path.
//!
//! (Mapping buffers into the IOAS via `IOMMU_IOAS_MAP` is not possible here
//! anyway: the page pool backs buffers with an mmap of `/dev/mshv_vtl_low`, a
//! `VM_PFNMAP` device mapping with no `struct page`, which GUP cannot
//! long-term pin, so the map ioctl fails with `EEXIST`.)

#![cfg(target_os = "linux")]
#![cfg(feature = "vfio")]

use crate::memory::MemoryBlock;
use anyhow::Context;
use std::sync::Arc;
use vfio_sys::iommufd::IommufdCtx;

/// A shared handle to an iommufd context and a noiommu IOAS for a cdev-based
/// VFIO device.
///
/// The same context and IOAS id must be passed to the cdev-based `VfioDevice`
/// so the device is attached to this IOAS.
pub struct IommufdIoas {
    ctx: Arc<IommufdCtx>,
    ioas_id: u32,
}

impl IommufdIoas {
    /// Opens a fresh iommufd context (`/dev/iommu`) and allocates a new
    /// noiommu IOAS, returning a handle for a single cdev-based VFIO device.
    pub fn new_noiommu() -> anyhow::Result<Arc<Self>> {
        let ctx = Arc::new(IommufdCtx::new().context("failed to open iommufd")?);
        let ioas_id = ctx
            .ioas_alloc()
            .context("failed to allocate noiommu IOAS")?;
        Ok(Arc::new(Self { ctx, ioas_id }))
    }

    /// The iommufd context backing this IOAS.
    pub fn ctx(&self) -> Arc<IommufdCtx> {
        self.ctx.clone()
    }

    /// The id of the IOAS the device is attached to.
    pub fn ioas_id(&self) -> u32 {
        self.ioas_id
    }

    /// Returns `block` unchanged so the device is programmed directly with the
    /// page pool's physical page numbers.
    ///
    /// In noiommu mode the attached cdev device performs untranslated DMA
    /// directly to physical addresses, and the page pool already reports the
    /// correct device PFNs (and `pfn_bias`) for the block, exactly as the
    /// legacy VFIO noiommu path programmed them. Individual buffers are
    /// therefore not mapped into the IOAS (which would fail anyway: the pool's
    /// `VM_PFNMAP` backing has no `struct page` for GUP to pin, so
    /// `IOMMU_IOAS_MAP` returns `EEXIST`).
    pub fn map_block(self: &Arc<Self>, block: MemoryBlock) -> anyhow::Result<MemoryBlock> {
        Ok(block)
    }
}
