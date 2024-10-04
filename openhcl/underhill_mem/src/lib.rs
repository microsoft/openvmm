// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![cfg(target_os = "linux")]
#![warn(missing_docs)]

//! Underhill VM memory management.

mod init;
mod registrar;

pub use init::init;
pub use init::Init;
pub use init::MemoryMappings;

mod mapping {
    // UNSAFETY: Implementing GuestMemoryAccess.
    #![allow(unsafe_code)]

    use crate::registrar::MemoryRegistrar;
    use guestmem::ranges::PagedRange;
    use guestmem::GuestMemoryAccess;
    use guestmem::GuestMemoryBackingError;
    use guestmem::PAGE_SIZE;
    use hcl::ioctl::AcceptPagesError;
    use hcl::ioctl::ApplyVtlProtectionsError;
    use hcl::ioctl::IsolationType;
    use hcl::ioctl::Mshv;
    use hcl::ioctl::MshvHvcall;
    use hcl::ioctl::MshvVtl;
    use hcl::ioctl::MshvVtlLow;
    use hcl::GuestVtl;
    use hv1_emulator::hv::VtlProtectHypercallOverlay;
    use hvdef::hypercall::AcceptMemoryType;
    use hvdef::hypercall::HostVisibilityType;
    use hvdef::hypercall::HvInputVtl;
    use hvdef::HvError;
    use hvdef::HvMapGpaFlags;
    use hvdef::HvRepResult;
    use hvdef::HypercallCode;
    use hvdef::Vtl;
    use hvdef::HV_MAP_GPA_PERMISSIONS_ALL;
    use hvdef::HV_PAGE_SIZE;
    use inspect::Inspect;
    use memory_range::MemoryRange;
    use parking_lot::Mutex;
    use sparse_mmap::SparseMapping;
    use std::ptr::NonNull;
    use std::sync::Arc;
    use thiserror::Error;
    use virt_mshv_vtl::ProtectIsolatedMemory;
    use vm_topology::memory::MemoryLayout;
    use vtl_array::VtlArray;
    use x86defs::snp::SevRmpAdjust;
    use x86defs::tdx::GpaVmAttributes;
    use x86defs::tdx::TdgMemPageAttrWriteR8;
    use x86defs::tdx::TdgMemPageGpaAttr;

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
    #[allow(missing_docs)]
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

    #[derive(Debug)]
    struct MshvVtlWithPolicy {
        mshv_vtl: MshvVtl,
        ignore_registration_failure: bool,
    }

    impl crate::registrar::RegisterMemory for MshvVtlWithPolicy {
        fn register_range(
            &self,
            range: MemoryRange,
        ) -> Result<(), impl 'static + std::error::Error> {
            match self.mshv_vtl.add_vtl0_memory(range) {
                Ok(()) => Ok(()),
                // TODO: remove this once the kernel driver tracks registration
                Err(err) if self.ignore_registration_failure => {
                    tracing::warn!(
                        error = &err as &dyn std::error::Error,
                        "registration failure, could be expected"
                    );
                    Ok(())
                }
                Err(err) => Err(err),
            }
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

        fn check_bitmap(&self, gpn: u64) -> bool {
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

    #[derive(Debug, Error)]
    #[error("failed to register memory with kernel")]
    struct RegistrationError;

    /// Currently built for hardware CVMs, which only define permissions for VTL
    /// 0 and VTL 1 to express what those VTLs have access to. If this were to
    /// extend to non-hardware CVMs, those would need to define permissions
    /// instead for VTL 2 and VTL 1 to express what the lower VTLs have access
    /// to.
    ///
    /// Default VTL memory permissions applied to any mapped memory
    struct DefaultVtlPermissions {
        vtl0: HvMapGpaFlags,
        vtl1: Option<HvMapGpaFlags>,
    }

    impl DefaultVtlPermissions {
        fn set(&mut self, vtl: Vtl, permissions: HvMapGpaFlags) {
            match vtl {
                Vtl::Vtl0 => self.vtl0 = permissions,
                Vtl::Vtl1 => self.vtl1 = Some(permissions),
                Vtl::Vtl2 => unreachable!(),
            }
        }
    }

    /// Represents the vtl permissions on a page for a given isolation type
    #[derive(Copy, Clone)]
    enum GpaVtlPermissions {
        Vbs(HvMapGpaFlags),
        Snp(SevRmpAdjust),
        Tdx((TdgMemPageGpaAttr, TdgMemPageAttrWriteR8)),
    }

    impl GpaVtlPermissions {
        fn new(isolation: IsolationType, vtl: Vtl, protections: HvMapGpaFlags) -> Self {
            match isolation {
                IsolationType::Vbs => GpaVtlPermissions::Vbs(protections),
                IsolationType::Snp => {
                    let mut vtl_permissions = GpaVtlPermissions::Snp(SevRmpAdjust::new());
                    vtl_permissions.set(vtl, protections);
                    vtl_permissions
                }
                IsolationType::Tdx => {
                    let mut vtl_permissions = GpaVtlPermissions::Tdx((
                        TdgMemPageGpaAttr::new(),
                        TdgMemPageAttrWriteR8::new(),
                    ));
                    vtl_permissions.set(vtl, protections);
                    vtl_permissions
                }
            }
        }

        fn set(&mut self, vtl: Vtl, protections: HvMapGpaFlags) {
            match self {
                GpaVtlPermissions::Vbs(flags) => *flags = protections,
                GpaVtlPermissions::Snp(rmpadjust) => {
                    *rmpadjust = SevRmpAdjust::new()
                        .with_enable_read(protections.readable())
                        .with_enable_write(protections.writable())
                        .with_enable_user_execute(protections.user_executable())
                        .with_enable_kernel_execute(protections.kernel_executable())
                        .with_target_vmpl(match vtl {
                            Vtl::Vtl0 => x86defs::snp::Vmpl::Vmpl2.into(),
                            Vtl::Vtl1 => x86defs::snp::Vmpl::Vmpl1.into(),
                            Vtl::Vtl2 => unreachable!(), // Cannot set VTL 2 protections
                        });
                }
                GpaVtlPermissions::Tdx((attributes, mask)) => {
                    let vm_attributes = GpaVmAttributes::new()
                        .with_valid(true)
                        .with_read(protections.readable())
                        .with_write(protections.writable())
                        .with_kernel_execute(protections.kernel_executable())
                        .with_user_execute(protections.user_executable());

                    let (new_attributes, new_mask) = match vtl {
                        Vtl::Vtl0 => {
                            let attributes = TdgMemPageGpaAttr::new().with_l2_vm1(vm_attributes);
                            let mask =
                                TdgMemPageAttrWriteR8::new().with_l2_vm1(vm_attributes.to_mask());
                            (attributes, mask)
                        }
                        Vtl::Vtl1 => {
                            let attributes = TdgMemPageGpaAttr::new().with_l2_vm2(vm_attributes);
                            let mask =
                                TdgMemPageAttrWriteR8::new().with_l2_vm2(vm_attributes.to_mask());
                            (attributes, mask)
                        }
                        Vtl::Vtl2 => unreachable!(),
                    };

                    *attributes = new_attributes;
                    *mask = new_mask;
                }
            }
        }
    }

    /// Interface to accept and manipulate lower VTL memory acceptance and page
    /// protections.
    ///
    /// FUTURE: this should go away as a separate object once all the logic is moved
    /// into this crate.
    pub struct MemoryAcceptor {
        mshv_hvcall: MshvHvcall,
        mshv_vtl: MshvVtl,
        isolation: IsolationType,
    }

    impl MemoryAcceptor {
        /// Create a new instance.
        pub fn new(isolation: IsolationType) -> Result<Self, hcl::ioctl::Error> {
            let mshv = Mshv::new()?;
            let mshv_vtl = mshv.create_vtl()?;
            let mshv_hvcall = MshvHvcall::new()?;
            mshv_hvcall.set_allowed_hypercalls(&[
                HypercallCode::HvCallAcceptGpaPages,
                HypercallCode::HvCallModifySparseGpaPageHostVisibility,
                HypercallCode::HvCallModifyVtlProtectionMask,
            ]);

            // On boot, VTL 0 should have permissions.
            Ok(Self {
                mshv_hvcall,
                mshv_vtl,
                isolation,
            })
        }

        /// Accept pages for VTL0.
        pub fn accept_vtl0_pages(&self, range: MemoryRange) -> Result<(), AcceptPagesError> {
            match self.isolation {
                IsolationType::Vbs => self
                    .mshv_hvcall
                    .accept_gpa_pages(range, AcceptMemoryType::RAM),
                IsolationType::Snp => {
                    self.mshv_vtl
                        .pvalidate_pages(range, true, false)
                        .map_err(|err| AcceptPagesError::Snp {
                            failed_operation: err,
                            range,
                        })
                }

                IsolationType::Tdx => {
                    let attributes =
                        TdgMemPageGpaAttr::new().with_l2_vm1(GpaVmAttributes::FULL_ACCESS);
                    let mask = TdgMemPageAttrWriteR8::new()
                        .with_l2_vm1(GpaVmAttributes::FULL_ACCESS.to_mask());

                    self.mshv_vtl
                        .tdx_accept_pages(range, Some((attributes, mask)))
                        .map_err(|err| AcceptPagesError::Tdx { error: err, range })
                }
            }
        }

        fn unaccept_vtl0_pages(&self, range: MemoryRange) {
            match self.isolation {
                IsolationType::Vbs => {
                    // TODO VBS: is there something to do here?
                }
                IsolationType::Snp => self
                    .mshv_vtl
                    .pvalidate_pages(range, false, false)
                    .expect("pvalidate should not fail"),
                IsolationType::Tdx => {
                    // Nothing to do for TDX.
                }
            }
        }

        /// Tell the host to change the visibility of the given GPAs.
        pub fn modify_gpa_visibility(
            &self,
            host_visibility: HostVisibilityType,
            gpns: &[u64],
        ) -> Result<(), HvError> {
            self.mshv_hvcall
                .modify_gpa_visibility(host_visibility, gpns)
        }

        /// Apply the initial protections on lower-vtl memory.
        ///
        ///  After initialization, the default protections should be applied.
        pub fn apply_initial_lower_vtl_protections(
            &self,
            range: MemoryRange,
        ) -> Result<(), ApplyVtlProtectionsError> {
            self.apply_protections_from_flags(range, Vtl::Vtl0, HV_MAP_GPA_PERMISSIONS_ALL)
        }

        /// Query the current permissions for a vtl on a page.
        fn vtl_permissions(&self, vtl: Vtl, gpa: u64) -> GpaVtlPermissions {
            match self.isolation {
                IsolationType::Vbs => unimplemented!(),
                IsolationType::Snp => {
                    // TODO CVM GUEST VSM: track the permissions directly in
                    // underhill. For now, use rmpquery.
                    assert!(self.isolation == IsolationType::Snp);
                    let rmpadjust = self.mshv_vtl.rmpquery_page(gpa, vtl);

                    GpaVtlPermissions::Snp(rmpadjust)
                }
                IsolationType::Tdx => todo!(),
            }
        }

        fn apply_protections_from_flags(
            &self,
            range: MemoryRange,
            vtl: Vtl,
            flags: HvMapGpaFlags,
        ) -> Result<(), ApplyVtlProtectionsError> {
            let permissions = GpaVtlPermissions::new(self.isolation, vtl, flags);
            self.apply_protections(range, vtl, permissions)
        }

        fn apply_protections(
            &self,
            range: MemoryRange,
            vtl: Vtl,
            protections: GpaVtlPermissions,
        ) -> Result<(), ApplyVtlProtectionsError> {
            match protections {
                GpaVtlPermissions::Vbs(flags) => {
                    // For VBS-isolated VMs, the permissions apply to all lower
                    // VTLs. Therefore VTL 0 cannot set its own permissions.
                    assert_ne!(vtl, Vtl::Vtl0);

                    self.mshv_hvcall
                        .modify_vtl_protection_mask(range, flags, HvInputVtl::from(vtl))
                }
                GpaVtlPermissions::Snp(rmpadjust) => {
                    // For SNP VMs, the permissions apply to the specified VTL.
                    // Therefore VTL 2 cannot specify its own permissions.
                    assert_ne!(vtl, Vtl::Vtl2);
                    self.mshv_vtl
                        .rmpadjust_pages(range, rmpadjust, false)
                        .map_err(|err| ApplyVtlProtectionsError::Snp {
                            failed_operation: err,
                            range,
                            permissions: rmpadjust,
                            vtl: vtl.into(),
                        })
                    // TODO SNP: Flush TLB
                }
                GpaVtlPermissions::Tdx((attributes, mask)) => {
                    // For TDX VMs, the permissions apply to the specified VTL.
                    // Therefore VTL 2 cannot specify its own permissions.
                    assert_ne!(vtl, Vtl::Vtl2);
                    self.mshv_vtl
                        .tdx_set_page_attributes(range, attributes, mask)
                        .map_err(|err| ApplyVtlProtectionsError::Tdx {
                            error: err,
                            range,
                            permissions: attributes,
                            vtl: vtl.into(),
                        })
                }
            }
        }
    }

    /// An implementation of [`virt_mshv_vtl::ChangeHostVisibility`] for Underhill VMs.
    pub struct HardwareIsolatedMemoryProtector {
        // Serves as a lock for synchronizing visibility and page-protection changes.
        inner: Mutex<HardwareIsolatedMemoryProtectorInner>,
        layout: MemoryLayout,
        acceptor: Arc<MemoryAcceptor>,
        hypercall_overlay: VtlArray<Arc<Mutex<Option<HypercallOverlay>>>, 2>,
    }

    struct HypercallOverlayProtector {
        vtl: Vtl,
        protector: Arc<dyn ProtectIsolatedMemory>,
    }

    struct HypercallOverlay {
        gpn: u64,
        permissions: GpaVtlPermissions,
    }

    impl VtlProtectHypercallOverlay for HypercallOverlayProtector {
        fn change_overlay(&self, gpn: u64) {
            self.protector.change_hypercall_overlay(self.vtl, gpn)
        }

        fn disable_overlay(&self) {
            self.protector.disable_hypercall_overlay(self.vtl)
        }
    }

    struct HardwareIsolatedMemoryProtectorInner {
        shared: Arc<GuestMemoryMapping>,
        encrypted: Arc<GuestMemoryMapping>,
        default_vtl_permissions: DefaultVtlPermissions,
    }

    impl HardwareIsolatedMemoryProtector {
        /// Returns a new instance.
        ///
        /// `shared` provides the mapping for shared memory. `vtl0` provides the
        /// mapping for encrypted memory.
        pub fn new(
            shared: Arc<GuestMemoryMapping>,
            encrypted: Arc<GuestMemoryMapping>,
            layout: MemoryLayout,
            acceptor: Arc<MemoryAcceptor>,
        ) -> Self {
            Self {
                inner: Mutex::new(HardwareIsolatedMemoryProtectorInner {
                    shared,
                    encrypted,
                    // Grant only VTL 0 all permissions. This will be altered
                    // later by VTL 1 enablement and by VTL 1 itself.
                    default_vtl_permissions: DefaultVtlPermissions {
                        vtl0: HV_MAP_GPA_PERMISSIONS_ALL,
                        vtl1: None,
                    },
                }),
                layout,
                acceptor,
                hypercall_overlay: VtlArray::from_fn(|_| Arc::new(Mutex::new(None))),
            }
        }

        fn apply_protections_with_overlay_handling(
            &self,
            vtl: Vtl,
            ranges: &[MemoryRange],
            protections: HvMapGpaFlags,
        ) -> Result<(), ApplyVtlProtectionsError> {
            // The overlay page cannot change over the course of this operation
            let mut overlay_lock = self.hypercall_overlay[vtl].lock();
            for range in ranges {
                match overlay_lock.as_mut() {
                    Some(overlay) if range.contains_addr(overlay.gpn * HV_PAGE_SIZE) => {
                        overlay.permissions.set(vtl, protections);

                        let overlay_address = overlay.gpn * HV_PAGE_SIZE;
                        let overlay_offset = range.offset_of(overlay_address).unwrap();
                        let (left, right) = range.split_at_offset(overlay_offset);

                        self.acceptor
                            .apply_protections_from_flags(left, vtl, protections)?;
                        let sub_range =
                            MemoryRange::new((overlay.gpn + 1) * HV_PAGE_SIZE..right.end());
                        if !sub_range.is_empty() {
                            self.acceptor.apply_protections_from_flags(
                                sub_range,
                                vtl,
                                protections,
                            )?;
                        }
                    }
                    _ => {
                        self.acceptor
                            .apply_protections_from_flags(*range, vtl, protections)?;
                    }
                }
            }
            Ok(())
        }

        /// Restore the original protections on the page that is overlaid.
        fn restore_overlay_permissions(
            &self,
            vtl: Vtl,
            overlay: &HypercallOverlay,
        ) -> Result<(), ApplyVtlProtectionsError> {
            let range =
                MemoryRange::new(overlay.gpn * HV_PAGE_SIZE..(overlay.gpn + 1) * HV_PAGE_SIZE);

            self.acceptor
                .apply_protections(range, vtl, overlay.permissions)?;

            Ok(())
        }
    }

    impl ProtectIsolatedMemory for HardwareIsolatedMemoryProtector {
        fn change_host_visibility(&self, shared: bool, gpns: &[u64]) -> HvRepResult {
            // Validate the ranges are RAM.
            for &gpn in gpns {
                if !self
                    .layout
                    .ram()
                    .iter()
                    .any(|r| r.range.contains_addr(gpn * HV_PAGE_SIZE))
                {
                    return Err((HvError::OperationDenied, 0));
                }

                // Don't allow the hypercall overlay to have shared visibility.
                if shared {
                    for vtl in [Vtl::Vtl1, Vtl::Vtl0] {
                        let overlay = self.hypercall_overlay[vtl].lock();
                        if let Some(overlay) = &*overlay {
                            if overlay.gpn == gpn {
                                return Err((HvError::OperationDenied, 0));
                            }
                        }
                    }
                }
            }

            let inner = self.inner.lock();

            // Filter out the GPNs that are already in the correct state.
            let orig_gpns = gpns;
            let gpns = gpns
                .iter()
                .copied()
                .filter(|&gpn| inner.shared.check_bitmap(gpn) != shared)
                .collect::<Vec<_>>();

            tracing::debug!(
                orig = orig_gpns.len(),
                len = gpns.len(),
                first = gpns.first(),
                shared,
                "change vis"
            );

            let ranges = PagedRange::new(0, gpns.len() * PagedRange::PAGE_SIZE, &gpns)
                .unwrap()
                .ranges()
                .map(|r| r.map(|r| MemoryRange::new(r.start..r.end)))
                .collect::<Result<Vec<_>, _>>()
                .unwrap(); // Ok to unwrap, we've validated the gpns above.

            // Prevent accesses via the wrong address.
            let clear_bitmap = if shared {
                &inner.encrypted
            } else {
                &inner.shared
            };
            for &range in &ranges {
                clear_bitmap.update_bitmap(range, false);
            }

            // TODO SNP: flush concurrent accessors and TLB.

            // TODO SNP: check list of locks, roll back bitmap changes if there was one.

            if shared {
                // Unaccept the pages so that the hypervisor can reclaim them.
                for &range in &ranges {
                    self.acceptor.unaccept_vtl0_pages(range);
                }
            }

            // Ask the hypervisor to update visibility.
            let host_visibility = if shared {
                HostVisibilityType::SHARED
            } else {
                HostVisibilityType::PRIVATE
            };
            if let Err(err) = self.acceptor.modify_gpa_visibility(host_visibility, &gpns) {
                if shared {
                    panic!("the hypervisor refused to transition pages to shared, we cannot safely roll back: {:?}", err);
                }
                todo!("roll back bitmap changes and report partial success");
            }

            if !shared {
                // Accept the pages so that the guest can use them.
                for &range in &ranges {
                    self.acceptor
                        .accept_vtl0_pages(range)
                        .expect("everything should be in a state where we can accept VTL0 pages");

                    // For SNP, zero the memory before allowing the guest to access
                    // them. For TDX, this is done by the TDX module. For mshv, this is
                    // done by the hypervisor.
                    if self.acceptor.isolation == IsolationType::Snp {
                        inner.encrypted
                    .mapping
                    .fill_at(range.start() as usize, 0, range.len() as usize)
                    .expect("VTL 2 should have access to lower VTL memory and the page should be accepted");
                    }
                }
            }

            // Allow accesses via the correct address.
            let set_bitmap = if shared {
                &inner.shared
            } else {
                &inner.encrypted
            };
            for &range in &ranges {
                set_bitmap.update_bitmap(range, true);
            }

            if !shared {
                // Apply vtl protections so that the guest can use them. The
                // hypercall overlay should not be host visible, so just apply
                // the default protections directly without handling of the
                // hypercall overlay.
                for &range in &ranges {
                    self.acceptor
                        .apply_protections_from_flags(
                            range,
                            Vtl::Vtl0,
                            inner.default_vtl_permissions.vtl0,
                        )
                        .expect("should be able to apply default protections");

                    if let Some(vtl1_protections) = inner.default_vtl_permissions.vtl1 {
                        self.acceptor
                            .apply_protections_from_flags(range, Vtl::Vtl1, vtl1_protections)
                            .expect("everything should be in a state where we can apply VTL protections");
                    }
                }
            }

            Ok(())
        }

        fn query_host_visibility(
            &self,
            gpns: &[u64],
            host_visibility: &mut [HostVisibilityType],
        ) -> HvRepResult {
            // Validate the ranges are RAM.
            for (i, &gpn) in gpns.iter().enumerate() {
                if !self
                    .layout
                    .ram()
                    .iter()
                    .any(|r| r.range.contains_addr(gpn * HV_PAGE_SIZE))
                {
                    return Err((HvError::OperationDenied, i));
                }
            }

            let inner = self.inner.lock();

            // Set GPN sharing status in output.
            for (gpn, host_vis) in gpns.iter().zip(host_visibility.iter_mut()) {
                *host_vis = if inner.shared.check_bitmap(*gpn) {
                    HostVisibilityType::SHARED
                } else {
                    HostVisibilityType::PRIVATE
                };
            }
            Ok(())
        }

        fn default_vtl0_protections(&self) -> HvMapGpaFlags {
            self.inner.lock().default_vtl_permissions.vtl0
        }

        fn change_default_vtl_protections(
            &self,
            vtl: GuestVtl,
            vtl_protections: HvMapGpaFlags,
        ) -> Result<(), HvError> {
            // Prevent visibility changes while VTL protections are being
            // applied.
            //
            // TODO: This does not need to be synchronized against other
            // threads performing VTL protection changes; whichever thread
            // finishes last will control the outcome.
            //
            // TODO GUEST VSM: Changes to vtl protections will need to be
            // synchronized with any checks for VTL protections (e.g. rmpquery)
            let mut inner = self.inner.lock();

            inner.default_vtl_permissions.set(vtl, vtl_protections);

            let mut ranges = Vec::new();
            for ram_range in self.layout.ram().iter() {
                let mut protect_start = ram_range.range.start();
                let mut page_count = 0;

                for gpn in ram_range.range.start() / PAGE_SIZE as u64
                    ..ram_range.range.end() / PAGE_SIZE as u64
                {
                    // TODO GUEST_VSM: for now, use the encrypted mapping to
                    // find all accepted memory. When lazy acceptance exists,
                    // this should track all pages that have been accepted and
                    // should be used instead.
                    if !inner.encrypted.check_bitmap(gpn) {
                        if page_count > 0 {
                            let end_address = protect_start + (page_count * PAGE_SIZE as u64);
                            ranges.push(MemoryRange::new(protect_start..end_address));
                        }
                        protect_start = (gpn + 1) * PAGE_SIZE as u64;
                        page_count = 0;
                    } else {
                        page_count += 1;
                    }
                }

                if page_count > 0 {
                    let end_address = protect_start + (page_count * PAGE_SIZE as u64);
                    ranges.push(MemoryRange::new(protect_start..end_address));
                }
            }

            self.apply_protections_with_overlay_handling(vtl, &ranges, vtl_protections)
                .expect("applying vtl protections should succeed");

            Ok(())
        }

        fn change_vtl_protections(
            &self,
            vtl: Vtl,
            gpns: &[u64],
            protections: HvMapGpaFlags,
        ) -> HvRepResult {
            assert!(vtl < Vtl::Vtl2);
            // Validate the ranges are RAM.
            for &gpn in gpns {
                if !self
                    .layout
                    .ram()
                    .iter()
                    .any(|r| r.range.contains_addr(gpn * HV_PAGE_SIZE))
                {
                    return Err((HvError::OperationDenied, 0));
                }
            }

            // Prevent visibility changes while VTL protections are being
            // applied. This does not need to be synchronized against other
            // threads performing VTL protection changes; whichever thread
            // finishes last will control the outcome.
            let inner = self.inner.lock();

            // Protections cannot be applied to a host-visible page
            if gpns.iter().any(|&gpn| inner.shared.check_bitmap(gpn)) {
                return Err((HvError::OperationDenied, 0));
            }

            // TODO GUEST VSM: For hardware-isolated VMs, track vtl protections in a bitmap

            let ranges = PagedRange::new(0, gpns.len() * PagedRange::PAGE_SIZE, gpns)
                .unwrap()
                .ranges()
                .map(|r| r.map(|r| MemoryRange::new(r.start..r.end)))
                .collect::<Result<Vec<_>, _>>()
                .unwrap(); // Ok to unwrap, we've validated the gpns above.

            self.apply_protections_with_overlay_handling(vtl, &ranges, protections)
                .expect("applying vtl protections should succeed");

            // TODO CVM GUEST VSM: flush TLB and wait for the tlb lock
            Ok(())
        }

        fn hypercall_overlay_protector(
            self: Arc<Self>,
            vtl: Vtl,
        ) -> Box<dyn VtlProtectHypercallOverlay> {
            Box::new(HypercallOverlayProtector {
                vtl,
                protector: self.clone(),
            })
        }

        fn change_hypercall_overlay(&self, vtl: Vtl, gpn: u64) {
            // Should already have written contents to the page via the guest
            // memory object, confirming that this is a guest page
            assert!(self
                .layout
                .ram()
                .iter()
                .any(|r| r.range.contains_addr(gpn * HV_PAGE_SIZE)));

            let _lock = self.inner.lock();

            let mut overlay = self.hypercall_overlay[vtl].lock();

            // Restore permissions on the previous overlay
            if let Some(overlay) = overlay.as_ref() {
                self.restore_overlay_permissions(vtl, overlay)
                    .expect("applying vtl protections should succeed");
            }

            let current_permissions = match self.acceptor.isolation {
                IsolationType::Vbs => unreachable!(),
                IsolationType::Snp => self.acceptor.vtl_permissions(vtl, gpn * HV_PAGE_SIZE),
                IsolationType::Tdx => {
                    // TODO TDX GUEST VSM: implement acceptor.vtl_permissions
                    // For now, since guest vsm isn't enabled, this overlay can
                    // only exist for VTL 0, which can't change its own
                    // permissions. So assume that the permissions haven't
                    // changed since VTL 2 initialized guest memory and they're
                    // still all.
                    GpaVtlPermissions::new(IsolationType::Tdx, vtl, HV_MAP_GPA_PERMISSIONS_ALL)
                }
            };

            *overlay = Some(HypercallOverlay {
                gpn,
                permissions: current_permissions,
            });

            self.acceptor
                .apply_protections_from_flags(
                    MemoryRange::new(gpn * HV_PAGE_SIZE..(gpn + 1) * HV_PAGE_SIZE),
                    vtl,
                    HV_MAP_GPA_PERMISSIONS_ALL,
                )
                .expect("applying vtl protections should succeed");

            // TODO CVM GUEST VSM: flush TLB
        }

        fn disable_hypercall_overlay(&self, vtl: Vtl) {
            let _lock = self.inner.lock();

            let mut overlay = self.hypercall_overlay[vtl].lock();

            if let Some(overlay) = overlay.as_ref() {
                self.restore_overlay_permissions(vtl, overlay)
                    .expect("applying vtl protections should succeed");
            }

            *overlay = None;

            // TODO CVM GUEST VSM: flush TLB
        }
    }
}
