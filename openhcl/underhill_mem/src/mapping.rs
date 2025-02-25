// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Implementing GuestMemoryAccess.
#![expect(unsafe_code)]

use crate::registrar::MemoryRegistrar;
use crate::MshvVtlWithPolicy;
use crate::RegistrationError;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use guestmem::PAGE_SIZE;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtlLow;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use sparse_mmap::SparseMapping;
use std::ptr::NonNull;
use thiserror::Error;
use vm_topology::memory::MemoryLayout;

/// An implementation of a [`GuestMemoryAccess`] trait for Underhill VMs.
#[derive(Debug, Inspect)]
pub struct GuestMemoryMapping {
    #[inspect(skip)]
    mapping: SparseMapping,
    iova_offset: Option<u64>,
    #[inspect(with = "Option::is_some")]
    bitmap: Option<SparseMapping>,
    #[inspect(skip)]
    bitmap_lock: Mutex<()>,
    registrar: Option<MemoryRegistrar<MshvVtlWithPolicy>>,
}

/// Error constructing a [`GuestMemoryMapping`].
#[derive(Debug, Error)]
pub enum MappingError {
    #[error("failed to allocate VA space for guest memory")]
    Reserve(#[source] std::io::Error),
    #[error("failed to map guest memory pages")]
    Map(#[source] std::io::Error),
    #[error("failed to allocate VA space for bitmap")]
    BitmapReserve(#[source] std::io::Error),
    #[error("failed to map zero pages for bitmap")]
    BitmapMap(#[source] std::io::Error),
    #[error("failed to allocate pages for bitmap")]
    BitmapAlloc(#[source] std::io::Error),
    #[error("memory map entry {0} has insufficient alignment to support a bitmap")]
    BadAlignment(MemoryRange),
    #[error("failed to open device")]
    OpenDevice(#[source] hcl::ioctl::Error),
}

/// A builder for [`GuestMemoryMapping`].
pub struct GuestMemoryMappingBuilder {
    physical_address_base: u64,
    bitmap_state: Option<bool>,
    shared: bool,
    for_kernel_access: bool,
    dma_base_address: Option<u64>,
    ignore_registration_failure: bool,
}

impl GuestMemoryMappingBuilder {
    /// Set whether to allocate a tracking for memory access, and specify the
    /// initial state of the bitmap.
    ///
    /// This is used to support tracking the shared/encrypted state of each
    /// page.
    ///
    /// FUTURE: use bitmaps to track VTL permissions as well, to support guest
    /// VSM for hardware-isolated VMs.
    pub fn use_bitmap(&mut self, initial_state: Option<bool>) -> &mut Self {
        self.bitmap_state = initial_state;
        self
    }

    /// Set whether this is a mapping to access shared memory.
    pub fn shared(&mut self, is_shared: bool) -> &mut Self {
        self.shared = is_shared;
        self
    }

    /// Set whether this mapping's memory can be locked to pass to the kernel.
    ///
    /// If so, then the memory will be registered with the kernel as part of
    /// `expose_va`, which is called when memory is locked.
    pub fn for_kernel_access(&mut self, for_kernel_access: bool) -> &mut Self {
        self.for_kernel_access = for_kernel_access;
        self
    }

    /// Sets the base address to use for DMAs to this memory.
    ///
    /// This may be `None` if DMA is not supported.
    ///
    /// The address to use depends on the backing technology. For SNP VMs, it
    /// should be either zero or the VTOM address, since shared memory is mapped
    /// twice. For TDX VMs, shared memory is only mapped once, but the IOMMU
    /// expects the SHARED bit to be set in DMA transactions, so it should be
    /// set here. And for non-isolated/software-isolated VMs, it should be zero
    /// or the VTL0 alias address, depending on which VTL this memory mapping is
    /// for.
    pub fn dma_base_address(&mut self, dma_base_address: Option<u64>) -> &mut Self {
        self.dma_base_address = dma_base_address;
        self
    }

    /// Ignore registration failures when registering memory with the kernel.
    ///
    /// This should be used when user mode is restarted for servicing but the
    /// kernel is not. Since this is not currently a production scenario, this
    /// is a simple way to avoid needing to track the state of the kernel
    /// registration across user-mode restarts.
    ///
    /// It is not a good idea to enable this otherwise, since the kernel very
    /// noisily complains if memory is registered twice, so we don't want that
    /// leaking into production scenarios.
    ///
    /// FUTURE: fix the kernel to silently succeed duplication registrations.
    pub fn ignore_registration_failure(&mut self, ignore: bool) -> &mut Self {
        self.ignore_registration_failure = ignore;
        self
    }

    /// Map the lower VTL address space.
    ///
    /// If `is_shared`, then map the kernel mapping as shared memory.
    ///
    /// Add in `file_starting_offset` to construct the page offset for each
    /// memory range. This can be the high bit to specify decrypted/shared
    /// memory, or it can be the VTL0 alias map start for non-isolated VMs.
    ///
    /// When handing out IOVAs for device DMA, add `iova_offset`. This can be
    /// VTOM for SNP-isolated VMs, or it can be the VTL0 alias map start for
    /// non-isolated VMs.
    ///
    /// If `bitmap_state` is `Some`, a bitmap is created to track the
    /// accessibility state of each page in the lower VTL memory. The bitmap is
    /// initialized to the provided state.
    pub fn build(
        &self,
        mshv_vtl_low: &MshvVtlLow,
        memory_layout: &MemoryLayout,
    ) -> Result<GuestMemoryMapping, MappingError> {
        // Calculate the file offset within the `mshv_vtl_low` file.
        let file_starting_offset = self.physical_address_base
            | if self.shared {
                MshvVtlLow::SHARED_MEMORY_FLAG
            } else {
                0
            };

        // Calculate the total size of the address space by looking at the ending region.
        let last_entry = memory_layout
            .ram()
            .last()
            .expect("memory map must have at least 1 entry");
        let address_space_size = last_entry.range.end();
        let mapping =
            SparseMapping::new(address_space_size as usize).map_err(MappingError::Reserve)?;

        tracing::trace!(?mapping, "map_lower_vtl_memory mapping");

        let bitmap = if self.bitmap_state.is_some() {
            let bitmap = SparseMapping::new((address_space_size as usize / PAGE_SIZE + 7) / 8)
                .map_err(MappingError::BitmapReserve)?;
            bitmap
                .map_zero(0, bitmap.len())
                .map_err(MappingError::BitmapMap)?;
            Some(bitmap)
        } else {
            None
        };

        // Loop through each of the memory map entries and create a mapping for it.
        for entry in memory_layout.ram() {
            if entry.range.is_empty() {
                continue;
            }
            let base_addr = entry.range.start();
            let file_offset = file_starting_offset.checked_add(base_addr).unwrap();

            tracing::trace!(base_addr, file_offset, "mapping lower ram");

            mapping
                .map_file(
                    base_addr as usize,
                    entry.range.len() as usize,
                    mshv_vtl_low.get(),
                    file_offset,
                    true,
                )
                .map_err(MappingError::Map)?;

            if let Some(bitmap) = &bitmap {
                // To simplify bitmap implementation, require that all memory
                // regions be 8-page aligned. Relax this if necessary.
                if entry.range.start() % (PAGE_SIZE as u64 * 8) != 0
                    || entry.range.end() % (PAGE_SIZE as u64 * 8) != 0
                {
                    return Err(MappingError::BadAlignment(entry.range));
                }

                let bitmap_start = entry.range.start() as usize / PAGE_SIZE / 8;
                let bitmap_end = (entry.range.end() - 1) as usize / PAGE_SIZE / 8;
                let bitmap_page_start = bitmap_start / PAGE_SIZE;
                let bitmap_page_end = bitmap_end / PAGE_SIZE;
                let page_count = bitmap_page_end + 1 - bitmap_page_start;

                // TODO SNP: map some pre-reserved lower VTL memory into the
                // bitmap. Or just figure out how to hot add that memory to the
                // kernel. Or have the boot loader reserve it at boot time.
                bitmap
                    .alloc(bitmap_page_start * PAGE_SIZE, page_count * PAGE_SIZE)
                    .map_err(MappingError::BitmapAlloc)?;
            }

            tracing::trace!(?entry, "mapped memory map entry");
        }

        // Set the initial bitmap state.
        if let Some((bitmap, true)) = bitmap.as_ref().zip(self.bitmap_state) {
            for entry in memory_layout.ram() {
                let start_gpn = entry.range.start() / PAGE_SIZE as u64;
                let gpn_count = entry.range.len() / PAGE_SIZE as u64;
                assert_eq!(entry.range.start() % 8, 0);
                assert_eq!(gpn_count % 8, 0);
                bitmap
                    .fill_at(start_gpn as usize / 8, 0xff, gpn_count as usize / 8)
                    .unwrap();
            }
        }

        let registrar = if self.for_kernel_access {
            let mshv = Mshv::new().map_err(MappingError::OpenDevice)?;
            let mshv_vtl = mshv.create_vtl().map_err(MappingError::OpenDevice)?;
            Some(MemoryRegistrar::new(
                memory_layout,
                self.physical_address_base,
                MshvVtlWithPolicy {
                    mshv_vtl,
                    ignore_registration_failure: self.ignore_registration_failure,
                    shared: self.shared,
                },
            ))
        } else {
            None
        };

        Ok(GuestMemoryMapping {
            mapping,
            iova_offset: self.dma_base_address,
            bitmap,
            bitmap_lock: Default::default(),
            registrar,
        })
    }
}

impl GuestMemoryMapping {
    /// Create a new builder for a guest memory mapping.
    ///
    /// Map all ranges with a physical address offset of
    /// `physical_address_base`. This can be zero, or the VTOM address for SNP,
    /// or the VTL0 alias address for non-isolated/software-isolated VMs.
    pub fn builder(physical_address_base: u64) -> GuestMemoryMappingBuilder {
        GuestMemoryMappingBuilder {
            physical_address_base,
            bitmap_state: None,
            shared: false,
            for_kernel_access: false,
            dma_base_address: None,
            ignore_registration_failure: false,
        }
    }

    pub(crate) fn check_bitmap(&self, gpn: u64) -> bool {
        let bitmap = self.bitmap.as_ref().unwrap();
        let mut b = 0;
        bitmap
            .read_at(gpn as usize / 8, std::slice::from_mut(&mut b))
            .unwrap();
        b & (1 << (gpn % 8)) != 0
    }

    /// Panics if the range is outside of guest RAM.
    pub fn update_bitmap(&self, range: MemoryRange, state: bool) {
        let bitmap = self.bitmap.as_ref().unwrap();
        let _lock = self.bitmap_lock.lock();
        for gpn in range.start() / PAGE_SIZE as u64..range.end() / PAGE_SIZE as u64 {
            // TODO: use `fill_at` for the aligned part of the range.
            let mut b = 0;
            bitmap
                .read_at(gpn as usize / 8, std::slice::from_mut(&mut b))
                .unwrap();
            if state {
                b |= 1 << (gpn % 8);
            } else {
                b &= !(1 << (gpn % 8));
            }
            bitmap
                .write_at(gpn as usize / 8, std::slice::from_ref(&b))
                .unwrap();
        }
    }

    pub(crate) fn zero_range(
        &self,
        range: MemoryRange,
    ) -> Result<(), sparse_mmap::SparseMappingError> {
        self.mapping
            .fill_at(range.start() as usize, 0, range.len() as usize)
    }
}

/// SAFETY: Implementing the `GuestMemoryAccess` contract, including the
/// size and lifetime of the mappings and bitmaps.
unsafe impl GuestMemoryAccess for GuestMemoryMapping {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.mapping.as_ptr().cast())
    }

    fn max_address(&self) -> u64 {
        self.mapping.len() as u64
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        if let Some(registrar) = &self.registrar {
            registrar
                .register(address, len)
                .map_err(|start| GuestMemoryBackingError::new(start, RegistrationError))
        } else {
            // TODO: fail this call once we have a way to avoid calling this for
            // user-mode-only accesses to locked memory (e.g., for vmbus ring
            // buffers). We can't fail this for now because TDX cannot register
            // encrypted memory.
            Ok(())
        }
    }

    fn base_iova(&self) -> Option<u64> {
        // When the alias map is configured for this mapping, VTL2-mapped
        // devices need to do DMA with the alias map bit set to avoid DMAing
        // into VTL1 memory.
        self.iova_offset
    }

    fn access_bitmap(&self) -> Option<guestmem::BitmapInfo> {
        self.bitmap.as_ref().map(|bitmap| {
            let ptr = NonNull::new(bitmap.as_ptr().cast()).unwrap();
            guestmem::BitmapInfo {
                read_bitmap: ptr,
                write_bitmap: ptr,
                execute_bitmap: ptr,
                bit_offset: 0,
            }
        })
    }
}
