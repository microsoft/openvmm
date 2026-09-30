// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// UNSAFETY: Implementing GuestMemoryAccess.
#![expect(unsafe_code)]

use crate::MshvVtlWithPolicy;
use crate::RegistrationError;
use crate::registrar::MemoryRegistrar;
use crate::registrar::RegisterAllError;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use guestmem::PAGE_SIZE;
use hcl::GuestVtl;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvVtlLow;
use hvdef::HvMapGpaFlags;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use sparse_mmap::SparseMapping;
use std::ptr::NonNull;
use std::sync::Arc;
use thiserror::Error;
use virt_mshv_vtl::ProtectIsolatedMemory;
use vm_topology::memory::MemoryLayout;

pub struct GuestPartitionMemoryView<'a> {
    memory_layout: &'a MemoryLayout,
    valid_memory: Arc<GuestValidMemory>,
}

impl<'a> GuestPartitionMemoryView<'a> {
    /// A bitmap is created to track the accessibility state of each page in the
    /// lower VTL memory. The bitmap is initialized to valid_bitmap_state.
    ///
    /// This is used to support tracking the shared/encrypted state of each
    /// page.
    pub fn new(
        memory_layout: &'a MemoryLayout,
        memory_type: GuestValidMemoryType,
        valid_bitmap_state: bool,
    ) -> Result<Self, MappingError> {
        let valid_memory =
            GuestValidMemory::new(memory_layout, memory_type, valid_bitmap_state).map(Arc::new)?;
        Ok(Self {
            memory_layout,
            valid_memory,
        })
    }

    /// Returns the built partition-wide valid memory.
    pub fn partition_valid_memory(&self) -> Arc<GuestValidMemory> {
        self.valid_memory.clone()
    }

    /// Build a [`GuestMemoryMapping`], feeding in any related partition-wide
    /// state.
    fn build_guest_memory_mapping(
        &self,
        mshv_vtl_low: &MshvVtlLow,
        memory_mapping_builder: &mut GuestMemoryMappingBuilder,
    ) -> Result<GuestMemoryMapping, MappingError> {
        memory_mapping_builder
            .use_partition_valid_memory(Some(self.valid_memory.clone()))
            .build(mshv_vtl_low, self.memory_layout)
    }
}

#[derive(Debug, Inspect)]
pub enum GuestMemoryViewReadType {
    Read,
    KernelExecute,
    UserExecute,
}

#[derive(Inspect)]
pub struct GuestMemoryView {
    #[inspect(skip)]
    protector: Option<Arc<dyn ProtectIsolatedMemory>>,
    pub memory_mapping: Arc<GuestMemoryMapping>,
    pub view_type: GuestMemoryViewReadType,
    vtl: GuestVtl,
}

impl GuestMemoryView {
    pub fn new(
        protector: Option<Arc<dyn ProtectIsolatedMemory>>,
        memory_mapping: Arc<GuestMemoryMapping>,
        view_type: GuestMemoryViewReadType,
        vtl: GuestVtl,
    ) -> Self {
        Self {
            protector,
            memory_mapping,
            view_type,
            vtl,
        }
    }
}

#[derive(Error, Debug)]
#[error("the specified page is not mapped")]
struct NotMapped;

#[derive(Error, Debug)]
enum BitmapFailure {
    #[error("the specified page was accessed using the wrong visibility mapping")]
    IncorrectHostVisibilityAccess,
    #[error("the specified page access violates VTL 1 protections")]
    Vtl1ProtectionsViolation,
}

/// SAFETY: Implementing the `GuestMemoryAccess` contract, including the
/// size and lifetime of the mappings and bitmaps.
unsafe impl GuestMemoryAccess for GuestMemoryView {
    fn mapping(&self) -> Option<NonNull<u8>> {
        NonNull::new(self.memory_mapping.mapping.as_ptr().cast())
    }

    fn max_address(&self) -> u64 {
        self.memory_mapping.mapping.len() as u64
    }

    fn expose_va(&self, address: u64, len: u64) -> Result<(), GuestMemoryBackingError> {
        if let Some(registrar) = &self.memory_mapping.registrar {
            registrar
                .register(address, len)
                .map_err(|start| GuestMemoryBackingError::other(start, RegistrationError))
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
        self.memory_mapping.iova_offset
    }

    fn access_bitmap(&self) -> Option<guestmem::BitmapInfo> {
        // When the permissions bitmaps are available, they take precedence and
        // therefore should be no more permissive than the access bitmap.
        //
        // TODO GUEST VSM: consider being able to dynamically update these
        // bitmaps. There are two scenarios where this would be useful:
        // 1. To reduce memory consumption in cases where the bitmaps aren't
        //    needed, i.e. the guest chooses not to enable guest vsm and VTL 1
        //    gets revoked.
        // 2. Because the related guest memory objects are initialized before
        // VTL 1 is, the code as it currently stands will always enforce vtl 1
        // protections even if VTL 1 hasn't explicitly enabled it. e.g. if VTL 1
        // never enables vtl protections via the vsm partition config, but it
        // still makes hypercalls to modify the vtl protection mask (this is a
        // valid scenario to help set up default protections), these protections
        // will still be enforced. In practice, a well-designed VTL 1 probably
        // would enable vtl protections before allowing VTL 0 to run again, but
        // technically the implementation here is not to spec.
        if let Some(bitmaps) = self.memory_mapping.permission_bitmaps.as_ref() {
            match self.view_type {
                GuestMemoryViewReadType::Read => Some(guestmem::BitmapInfo {
                    read_bitmap: NonNull::new(bitmaps.read_bitmap.as_ptr().cast()).unwrap(),
                    write_bitmap: NonNull::new(bitmaps.write_bitmap.as_ptr().cast()).unwrap(),
                    bit_offset: 0,
                }),
                GuestMemoryViewReadType::KernelExecute => Some(guestmem::BitmapInfo {
                    read_bitmap: NonNull::new(bitmaps.kernel_execute_bitmap.as_ptr().cast())
                        .unwrap(),
                    write_bitmap: NonNull::new(bitmaps.write_bitmap.as_ptr().cast()).unwrap(),
                    bit_offset: 0,
                }),
                GuestMemoryViewReadType::UserExecute => Some(guestmem::BitmapInfo {
                    read_bitmap: NonNull::new(bitmaps.user_execute_bitmap.as_ptr().cast()).unwrap(),
                    write_bitmap: NonNull::new(bitmaps.write_bitmap.as_ptr().cast()).unwrap(),
                    bit_offset: 0,
                }),
            }
        } else {
            self.memory_mapping
                .valid_memory
                .as_ref()
                .map(|bitmap| bitmap.access_bitmap())
        }
    }

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> guestmem::PageFaultAction {
        let gpn = address / PAGE_SIZE as u64;
        if !bitmap_failure {
            guestmem::PageFaultAction::Fail(guestmem::PageFaultError::other(NotMapped {}))
        } else {
            let valid_memory = self
                .memory_mapping
                .valid_memory
                .as_ref()
                .expect("all backings with bitmaps should have a GuestValidMemory");
            if !valid_memory.check_valid(gpn) {
                match valid_memory.memory_type() {
                    GuestValidMemoryType::Shared => {
                        tracelimit::warn_ratelimited!(
                            ?address,
                            ?len,
                            ?write,
                            "tried to access private page using shared mapping"
                        );
                        guestmem::PageFaultAction::Fail(guestmem::PageFaultError::new(
                            guestmem::GuestMemoryErrorKind::NotShared,
                            BitmapFailure::IncorrectHostVisibilityAccess,
                        ))
                    }
                    GuestValidMemoryType::Encrypted => {
                        tracelimit::warn_ratelimited!(
                            ?address,
                            ?len,
                            ?write,
                            "tried to access shared page using private mapping"
                        );
                        guestmem::PageFaultAction::Fail(guestmem::PageFaultError::new(
                            guestmem::GuestMemoryErrorKind::NotPrivate,
                            BitmapFailure::IncorrectHostVisibilityAccess,
                        ))
                    }
                }
            } else {
                // Currently, only VTL 1 permissions are tracked, so any
                // invalid accesses here violate VTL 1 protections.
                if let Some(permission_bitmaps) = &self.memory_mapping.permission_bitmaps {
                    let check_bitmap = if write {
                        &permission_bitmaps.write_bitmap
                    } else {
                        match self.view_type {
                            GuestMemoryViewReadType::Read => &permission_bitmaps.read_bitmap,
                            GuestMemoryViewReadType::KernelExecute => {
                                &permission_bitmaps.kernel_execute_bitmap
                            }
                            GuestMemoryViewReadType::UserExecute => {
                                &permission_bitmaps.user_execute_bitmap
                            }
                        }
                    };

                    if !check_bitmap.page_state(gpn) {
                        tracelimit::warn_ratelimited!(?address, ?len, ?write, ?self.view_type, "VTL 1 permissions violation");

                        return guestmem::PageFaultAction::Fail(guestmem::PageFaultError::new(
                            guestmem::GuestMemoryErrorKind::VtlProtected,
                            BitmapFailure::Vtl1ProtectionsViolation,
                        ));
                    }
                }

                // Possible race condition where the bitmaps are in transition
                // and while the original check failed, the bitmaps now show
                // valid access to the page. Retry in that situation.
                guestmem::PageFaultAction::Retry
            }
        }
    }

    fn lock_gpns(&self, gpns: &[u64]) -> Result<bool, GuestMemoryBackingError> {
        if let Some(protector) = self.protector.as_ref() {
            protector.lock_gpns(self.vtl, gpns)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn unlock_gpns(&self, gpns: &[u64]) {
        if let Some(protector) = self.protector.as_ref() {
            protector.unlock_gpns(self.vtl, gpns)
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum GuestValidMemoryType {
    Shared,
    Encrypted,
}

/// Partition-wide (cross-vtl) tracking of valid memory that can be used in
/// individual GuestMemoryMappings.
#[derive(Debug)]
pub struct GuestValidMemory {
    valid_bitmap: GuestMemoryBitmap,
    valid_bitmap_lock: Mutex<()>,
    memory_type: GuestValidMemoryType,
}

impl GuestValidMemory {
    fn new(
        memory_layout: &MemoryLayout,
        memory_type: GuestValidMemoryType,
        valid_bitmap_state: bool,
    ) -> Result<Self, MappingError> {
        let valid_bitmap = {
            let mut bitmap = {
                // Calculate the total size of the address space by looking at the ending region.
                let last_entry = memory_layout
                    .ram()
                    .last()
                    .expect("memory map must have at least 1 entry");
                let address_space_size = last_entry.range.end();
                GuestMemoryBitmap::new(address_space_size as usize)?
            };

            for entry in memory_layout.ram() {
                if entry.range.is_empty() {
                    continue;
                }

                bitmap.init(entry.range, valid_bitmap_state)?;
            }

            bitmap
        };

        Ok(GuestValidMemory {
            valid_bitmap,
            valid_bitmap_lock: Default::default(),
            memory_type,
        })
    }

    /// Update the bitmap to reflect the validity of the given range.
    pub fn update_valid(&self, range: MemoryRange, state: bool) {
        let _lock = self.valid_bitmap_lock.lock();
        self.valid_bitmap.update(range, state);
    }

    /// Check if the given page is valid.
    pub(crate) fn check_valid(&self, gpn: u64) -> bool {
        self.valid_bitmap.page_state(gpn)
    }

    /// Returns the type of memory tracked by the bitmap
    pub(crate) fn memory_type(&self) -> GuestValidMemoryType {
        self.memory_type
    }

    fn access_bitmap(&self) -> guestmem::BitmapInfo {
        let ptr = NonNull::new(self.valid_bitmap.as_ptr()).unwrap();
        guestmem::BitmapInfo {
            read_bitmap: ptr,
            write_bitmap: ptr,
            bit_offset: 0,
        }
    }
}

/// An implementation of a [`GuestMemoryAccess`] trait for Underhill VMs.
#[derive(Debug, Inspect)]
pub struct GuestMemoryMapping {
    #[inspect(skip)]
    mapping: SparseMapping,
    iova_offset: Option<u64>,
    #[inspect(with = "Option::is_some")]
    valid_memory: Option<Arc<GuestValidMemory>>,
    #[inspect(with = "Option::is_some")]
    permission_bitmaps: Option<PermissionBitmaps>,
    registrar: Option<MemoryRegistrar<MshvVtlWithPolicy>>,
}

/// Bitmap implementation using sparse mapping that can be used to track page
/// states.
#[derive(Debug)]
struct PermissionBitmaps {
    permission_update_lock: Mutex<()>,
    read_bitmap: GuestMemoryBitmap,
    write_bitmap: GuestMemoryBitmap,
    kernel_execute_bitmap: GuestMemoryBitmap,
    user_execute_bitmap: GuestMemoryBitmap,
}

#[derive(Error, Debug)]
pub enum VtlPermissionsError {
    #[error("no vtl 1 permissions enforcement, bitmap is not present")]
    NoPermissionsTracked,
}

#[derive(Debug, Error)]
pub(crate) enum RegisterMemoryError {
    #[error("memory mapping is not configured for kernel registration")]
    NotConfigured,
    #[error(transparent)]
    Registration(#[from] RegisterAllError),
}

#[derive(Debug)]
struct GuestMemoryBitmap {
    bitmap: SparseMapping,
}

impl GuestMemoryBitmap {
    const PAGES_PER_CHUNK: u64 = 8;
    const BYTES_PER_CHUNK: u64 = PAGE_SIZE as u64 * Self::PAGES_PER_CHUNK;
    fn new(address_space_size: usize) -> Result<Self, MappingError> {
        let bitmap = SparseMapping::new(
            (address_space_size / PAGE_SIZE).div_ceil(Self::PAGES_PER_CHUNK as usize),
        )
        .map_err(MappingError::BitmapReserve)?;
        bitmap
            .map_zero(0, bitmap.len())
            .map_err(MappingError::BitmapMap)?;
        Ok(Self { bitmap })
    }

    fn init(&mut self, range: MemoryRange, state: bool) -> Result<(), MappingError> {
        if !range.start().is_multiple_of(Self::BYTES_PER_CHUNK)
            || !range.end().is_multiple_of(Self::BYTES_PER_CHUNK)
        {
            return Err(MappingError::BadAlignment(range));
        }

        let bitmap_start = (range.start() / Self::BYTES_PER_CHUNK) as usize;
        let bitmap_end = ((range.end() - 1) / Self::BYTES_PER_CHUNK) as usize;
        let bitmap_page_start = bitmap_start / PAGE_SIZE;
        let bitmap_page_end = bitmap_end / PAGE_SIZE;
        let page_count = bitmap_page_end + 1 - bitmap_page_start;

        // TODO CVM FUTURE: map some pre-reserved lower VTL memory into the
        // bitmap. Or just figure out how to hot add that memory to the
        // kernel. Or have the boot loader reserve it at boot time.
        //
        // Be careful, though--if we do that, we have to remember which
        // pages were already allocated and initialized; otherwise, we will
        // zero them again. To avoid tracking this as is, just update
        // the page protections on the existing zero-mapped pages.
        self.bitmap
            .set_writable(bitmap_page_start * PAGE_SIZE, page_count * PAGE_SIZE, true)
            .map_err(MappingError::BitmapProtect)?;

        // Set the initial bitmap state.
        if state {
            let start_gpn = range.start() / PAGE_SIZE as u64;
            let gpn_count = range.len() / PAGE_SIZE as u64;
            assert_eq!(range.start() % Self::PAGES_PER_CHUNK, 0);
            assert_eq!(gpn_count % Self::PAGES_PER_CHUNK, 0);
            self.bitmap
                .fill_at(
                    (start_gpn / Self::PAGES_PER_CHUNK) as usize,
                    0xff,
                    (gpn_count / Self::PAGES_PER_CHUNK) as usize,
                )
                .unwrap();
        }

        Ok(())
    }

    /// Panics if the range is outside of guest RAM.
    fn update(&self, range: MemoryRange, state: bool) {
        let aligned_range = range.aligned_subrange(Self::BYTES_PER_CHUNK);
        if !aligned_range.is_empty() {
            self.bitmap
                .fill_at(
                    (aligned_range.start() / Self::BYTES_PER_CHUNK) as usize,
                    if state { u8::MAX } else { 0u8 },
                    (aligned_range.len() / Self::BYTES_PER_CHUNK) as usize,
                )
                .unwrap();
        }

        for unaligned_range in memory_range::subtract_ranges([range], [aligned_range]) {
            for gpn in
                unaligned_range.start() / PAGE_SIZE as u64..unaligned_range.end() / PAGE_SIZE as u64
            {
                let mut b = 0;
                self.bitmap
                    .read_at(
                        (gpn / Self::PAGES_PER_CHUNK) as usize,
                        std::slice::from_mut(&mut b),
                    )
                    .unwrap();
                if state {
                    b |= 1 << (gpn % Self::PAGES_PER_CHUNK);
                } else {
                    b &= !(1 << (gpn % Self::PAGES_PER_CHUNK));
                }
                self.bitmap
                    .write_at(
                        (gpn / Self::PAGES_PER_CHUNK) as usize,
                        std::slice::from_ref(&b),
                    )
                    .unwrap();
            }
        }
    }

    /// Read the bitmap for `gpn`.
    /// Panics if the range is outside of guest RAM.
    fn page_state(&self, gpn: u64) -> bool {
        let mut b = 0;
        self.bitmap
            .read_at(
                (gpn / Self::PAGES_PER_CHUNK) as usize,
                std::slice::from_mut(&mut b),
            )
            .unwrap();
        b & (1 << (gpn % Self::PAGES_PER_CHUNK)) != 0
    }

    fn as_ptr(&self) -> *mut u8 {
        self.bitmap.as_ptr().cast()
    }
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
    #[error("failed to make pages writable for bitmap")]
    BitmapProtect(#[source] std::io::Error),
    #[error("memory map entry {0} has insufficient alignment to support a bitmap")]
    BadAlignment(MemoryRange),
    #[error("failed to open device")]
    OpenDevice(#[source] hcl::ioctl::Error),
}

/// A builder for [`GuestMemoryMapping`].
pub struct GuestMemoryMappingBuilder {
    physical_address_base: u64,
    valid_memory: Option<Arc<GuestValidMemory>>,
    permissions_bitmap_state: Option<bool>,
    shared: bool,
    kernel_registration_granularity: Option<u64>,
    dma_base_address: Option<u64>,
    ignore_registration_failure: bool,
}

impl GuestMemoryMappingBuilder {
    fn use_partition_valid_memory(
        &mut self,
        valid_memory: Option<Arc<GuestValidMemory>>,
    ) -> &mut Self {
        self.valid_memory = valid_memory;
        self
    }

    /// Set whether to allocate tracking bitmaps for memory access permissions,
    /// and specify the initial state of the bitmaps.
    ///
    /// This is used to support tracking the read/write/kernel execute/user
    /// execute permissions of each page.
    pub fn use_permissions_bitmaps(&mut self, initial_state: Option<bool>) -> &mut Self {
        self.permissions_bitmap_state = initial_state;
        self
    }

    /// Set whether this is a mapping to access shared memory.
    pub fn shared(&mut self, is_shared: bool) -> &mut Self {
        self.shared = is_shared;
        self
    }

    /// Allow this mapping's memory to be locked and passed to the kernel.
    ///
    /// Memory is registered in chunks of `registration_granularity` as part of
    /// `expose_va`, which is called when memory is locked.
    pub fn for_kernel_access(&mut self, registration_granularity: u64) -> &mut Self {
        self.kernel_registration_granularity = Some(registration_granularity);
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

    /// Mapping should leverage the bitmap used to track the accessibility state
    /// of each page in the lower VTL memory.
    pub fn build_with_bitmap(
        &mut self,
        mshv_vtl_low: &MshvVtlLow,
        partition_builder: &GuestPartitionMemoryView<'_>,
    ) -> Result<GuestMemoryMapping, MappingError> {
        partition_builder.build_guest_memory_mapping(mshv_vtl_low, self)
    }

    pub fn build_without_bitmap(
        &self,
        mshv_vtl_low: &MshvVtlLow,
        memory_layout: &MemoryLayout,
    ) -> Result<GuestMemoryMapping, MappingError> {
        self.build(mshv_vtl_low, memory_layout)
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
    fn build(
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

        let mut permission_bitmaps = if self.permissions_bitmap_state.is_some() {
            Some(PermissionBitmaps {
                permission_update_lock: Default::default(),
                read_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
                write_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
                kernel_execute_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
                user_execute_bitmap: GuestMemoryBitmap::new(address_space_size as usize)?,
            })
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

            if let Some((bitmaps, state)) = permission_bitmaps
                .as_mut()
                .zip(self.permissions_bitmap_state)
            {
                bitmaps.read_bitmap.init(entry.range, state)?;
                bitmaps.write_bitmap.init(entry.range, state)?;
                bitmaps.kernel_execute_bitmap.init(entry.range, state)?;
                bitmaps.user_execute_bitmap.init(entry.range, state)?;
            }

            tracing::trace!(?entry, "mapped memory map entry");
        }

        let registrar = if let Some(registration_granularity) = self.kernel_registration_granularity
        {
            let mshv = Mshv::new().map_err(MappingError::OpenDevice)?;
            let mshv_vtl = mshv.create_vtl().map_err(MappingError::OpenDevice)?;
            Some(MemoryRegistrar::new(
                memory_layout,
                self.physical_address_base,
                registration_granularity,
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
            valid_memory: self.valid_memory.clone(),
            permission_bitmaps,
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
            valid_memory: None,
            permissions_bitmap_state: None,
            shared: false,
            kernel_registration_granularity: None,
            dma_base_address: None,
            ignore_registration_failure: false,
        }
    }

    /// Register all complete `alignment`-aligned RAM subranges before they are accessed.
    ///
    /// If `ignore_unaligned_ranges` is false, fail if any RAM remains outside
    /// the aligned subranges.
    pub(crate) fn register_all_aligned_memory(
        &self,
        alignment: u64,
        ignore_unaligned_ranges: bool,
    ) -> Result<(), RegisterMemoryError> {
        self.registrar
            .as_ref()
            .ok_or(RegisterMemoryError::NotConfigured)?
            .register_all_aligned(alignment, ignore_unaligned_ranges)
            .map_err(RegisterMemoryError::from)
    }

    /// Update the permission bitmaps to reflect the given flags.
    /// Panics if the range is outside of guest RAM.
    pub fn update_permission_bitmaps(&self, range: MemoryRange, flags: HvMapGpaFlags) {
        if let Some(bitmaps) = self.permission_bitmaps.as_ref() {
            let _lock = bitmaps.permission_update_lock.lock();
            bitmaps.read_bitmap.update(range, flags.readable());
            bitmaps.write_bitmap.update(range, flags.writable());
            bitmaps
                .kernel_execute_bitmap
                .update(range, flags.kernel_executable());
            bitmaps
                .user_execute_bitmap
                .update(range, flags.user_executable());
        }
    }

    /// Query the permissions for the given gpn.
    /// Panics if the range is outside of guest RAM.
    pub fn query_access_permission(&self, gpn: u64) -> Result<HvMapGpaFlags, VtlPermissionsError> {
        if let Some(bitmaps) = self.permission_bitmaps.as_ref() {
            Ok(HvMapGpaFlags::new()
                .with_readable(bitmaps.read_bitmap.page_state(gpn))
                .with_writable(bitmaps.write_bitmap.page_state(gpn))
                .with_kernel_executable(bitmaps.kernel_execute_bitmap.page_state(gpn))
                .with_user_executable(bitmaps.user_execute_bitmap.page_state(gpn)))
        } else {
            Err(VtlPermissionsError::NoPermissionsTracked)
        }
    }

    /// Zero the given range of memory.
    pub(crate) fn zero_range(
        &self,
        range: MemoryRange,
    ) -> Result<(), sparse_mmap::SparseMappingError> {
        self.mapping
            .fill_at(range.start() as usize, 0, range.len() as usize)
    }
}

#[cfg(test)]
mod tests {
    use crate::mapping::GuestMemoryBitmap;
    use crate::mapping::GuestValidMemory;
    use crate::mapping::GuestValidMemoryType;
    use guestmem::PAGE_SIZE;
    use memory_range::MemoryRange;
    use vm_topology::memory::MemoryLayout;
    use vm_topology::memory::MemoryRangeWithNode;

    // Ensure a bitmap initialized with ranges whose bitmap backings overlap
    // does not cause any issues.
    #[test]
    fn test_overlapping_bitmap() {
        let memory_ranges = [0..1, 1..2, 3..4].map(|r| MemoryRangeWithNode {
            range: MemoryRange::from_4k_gpn_range(r.start * 8..r.end * 8),
            vnode: 0,
        });
        let memory_layout = MemoryLayout::new_from_ranges(&memory_ranges, &[])
            .expect("Failed to create memory layout");
        let guest_valid_mem =
            GuestValidMemory::new(&memory_layout, GuestValidMemoryType::Encrypted, true).unwrap();
        for (i, &b) in [true, true, false, true, false].iter().enumerate() {
            assert_eq!(guest_valid_mem.check_valid(i as u64 * 8), b, "{i}");
        }
    }

    #[test]
    fn test_bitmap_update() {
        const PAGE_COUNT: u64 = 24;

        // Cover fully-aligned, single-edge, both-edge, sub-byte, and
        // full-coverage ranges.
        for gpn_range in [8..16, 3..21, 5..6, 8..13, 3..8, 0..24, 7..17] {
            let mut bitmap = GuestMemoryBitmap::new(PAGE_COUNT as usize * PAGE_SIZE).unwrap();
            bitmap
                .init(MemoryRange::from_4k_gpn_range(0..PAGE_COUNT), false)
                .unwrap();

            let range = MemoryRange::from_4k_gpn_range(gpn_range.clone());
            bitmap.update(range, true);
            for gpn in 0..PAGE_COUNT {
                assert_eq!(bitmap.page_state(gpn), gpn_range.contains(&gpn), "{gpn}");
            }

            bitmap.update(range, false);
            for gpn in 0..PAGE_COUNT {
                assert!(!bitmap.page_state(gpn), "{gpn}");
            }
        }
    }

    // Apply a sequence of overlapping and partially-overlapping updates and
    // check the running state after each one.
    #[test]
    fn test_bitmap_update_overlapping() {
        const PAGE_COUNT: u64 = 24;

        let mut bitmap = GuestMemoryBitmap::new(PAGE_COUNT as usize * PAGE_SIZE).unwrap();
        bitmap
            .init(MemoryRange::from_4k_gpn_range(0..PAGE_COUNT), false)
            .unwrap();

        let mut expected = [false; PAGE_COUNT as usize];
        let ops = [
            (3..21, true),   // unaligned on both edges
            (0..4, false),   // clears the left chunk, shares an edge
            (6..10, true),   // partial overlap, re-sets some pages
            (9..24, true),   // shares the right edge
            (20..22, false), // clears an interior sub-byte span
        ];

        for (gpn_range, state) in ops {
            bitmap.update(MemoryRange::from_4k_gpn_range(gpn_range.clone()), state);
            for gpn in gpn_range.clone() {
                expected[gpn as usize] = state;
            }
            for gpn in 0..PAGE_COUNT {
                assert_eq!(
                    bitmap.page_state(gpn),
                    expected[gpn as usize],
                    "gpn {gpn} after updating {gpn_range:?} to {state}"
                );
            }
        }
    }
}
