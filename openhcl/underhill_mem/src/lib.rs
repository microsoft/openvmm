// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Underhill VM memory management.

#![cfg(target_os = "linux")]

mod init;
mod mapping;
mod registrar;

pub use init::BootInit;
pub use init::Init;
pub use init::MemoryMappings;
pub use init::init;

use cvm_tracing::CVM_ALLOWED;
use guestmem::PAGE_SIZE;
use guestmem::ranges::PagedRange;
use hcl::GuestVtl;
use hcl::ioctl::AcceptPagesError;
use hcl::ioctl::ApplyVtlProtectionsError;
use hcl::ioctl::Mshv;
use hcl::ioctl::MshvHvcall;
use hcl::ioctl::MshvVtl;
use hcl::ioctl::snp::SnpPageError;
use hv1_structs::VtlArray;
use hvdef::HV_MAP_GPA_PERMISSIONS_ALL;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvError;
use hvdef::HvMapGpaFlags;
use hvdef::HypercallCode;
use hvdef::Vtl;
use hvdef::hypercall::AcceptMemoryType;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvInputVtl;
use mapping::GuestMemoryMapping;
use mapping::GuestValidMemory;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use registrar::RegisterMemory;
use std::sync::Arc;
use thiserror::Error;
use virt::IsolationType;
use virt_mshv_vtl::ProtectIsolatedMemory;
use virt_mshv_vtl::TlbFlushLockAccess;
use vm_topology::memory::MemoryLayout;
use x86defs::snp::SevRmpAdjust;
use x86defs::tdx::GpaVmAttributes;
use x86defs::tdx::GpaVmAttributesMask;
use x86defs::tdx::TdgMemPageAttrWriteR8;
use x86defs::tdx::TdgMemPageGpaAttr;

/// Error querying vtl permissions on a page
#[derive(Debug, Error)]
pub enum QueryVtlPermissionsError {
    /// An SNP-specific error
    #[error("failed to query rmp permissions")]
    Snp(#[source] SnpPageError),
}

#[derive(Debug)]
struct MshvVtlWithPolicy {
    mshv_vtl: MshvVtl,
    ignore_registration_failure: bool,
    shared: bool,
}

impl RegisterMemory for MshvVtlWithPolicy {
    fn register_range(&self, range: MemoryRange) -> Result<(), impl 'static + std::error::Error> {
        match self.mshv_vtl.add_vtl0_memory(range, self.shared) {
            Ok(()) => Ok(()),
            // TODO: remove this once the kernel driver tracks registration
            Err(err) if self.ignore_registration_failure => {
                tracing::warn!(
                    CVM_ALLOWED,
                    error = &err as &dyn std::error::Error,
                    "registration failure, could be expected"
                );
                Ok(())
            }
            Err(err) => Err(err),
        }
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
    fn set(&mut self, vtl: GuestVtl, permissions: HvMapGpaFlags) {
        match vtl {
            GuestVtl::Vtl0 => self.vtl0 = permissions,
            GuestVtl::Vtl1 => self.vtl1 = Some(permissions),
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
    fn new(isolation: IsolationType, vtl: GuestVtl, protections: HvMapGpaFlags) -> Self {
        match isolation {
            IsolationType::None => unreachable!(),
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

    fn set(&mut self, vtl: GuestVtl, protections: HvMapGpaFlags) {
        match self {
            GpaVtlPermissions::Vbs(flags) => *flags = protections,
            GpaVtlPermissions::Snp(rmpadjust) => {
                *rmpadjust = SevRmpAdjust::new()
                    .with_enable_read(protections.readable())
                    .with_enable_write(protections.writable())
                    .with_enable_user_execute(protections.user_executable())
                    .with_enable_kernel_execute(protections.kernel_executable())
                    .with_target_vmpl(match vtl {
                        GuestVtl::Vtl0 => x86defs::snp::Vmpl::Vmpl2.into(),
                        GuestVtl::Vtl1 => x86defs::snp::Vmpl::Vmpl1.into(),
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
                    GuestVtl::Vtl0 => {
                        let attributes = TdgMemPageGpaAttr::new().with_l2_vm1(vm_attributes);
                        let mask = TdgMemPageAttrWriteR8::new()
                            .with_l2_vm1(GpaVmAttributesMask::ALL_CHANGED);
                        (attributes, mask)
                    }
                    GuestVtl::Vtl1 => {
                        let attributes = TdgMemPageGpaAttr::new().with_l2_vm2(vm_attributes);
                        let mask = TdgMemPageAttrWriteR8::new()
                            .with_l2_vm2(GpaVmAttributesMask::ALL_CHANGED);
                        (attributes, mask)
                    }
                };

                *attributes = new_attributes;
                *mask = new_mask;
            }
        }
    }
}

/// Error returned when modifying gpa visibility.
#[derive(Debug, Error)]
#[error("failed to modify gpa visibility, elements successfully processed {processed}")]
pub struct ModifyGpaVisibilityError {
    source: HvError,
    processed: usize,
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
            IsolationType::None => unreachable!(),
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
                let attributes = TdgMemPageGpaAttr::new().with_l2_vm1(GpaVmAttributes::FULL_ACCESS);
                let mask =
                    TdgMemPageAttrWriteR8::new().with_l2_vm1(GpaVmAttributesMask::ALL_CHANGED);

                self.mshv_vtl
                    .tdx_accept_pages(range, Some((attributes, mask)))
                    .map_err(|err| AcceptPagesError::Tdx { error: err, range })
            }
        }
    }

    fn unaccept_vtl0_pages(&self, range: MemoryRange) {
        match self.isolation {
            IsolationType::None => unreachable!(),
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
    ) -> Result<(), ModifyGpaVisibilityError> {
        self.mshv_hvcall
            .modify_gpa_visibility(host_visibility, gpns)
            .map_err(|(e, processed)| ModifyGpaVisibilityError {
                source: e,
                processed,
            })
    }

    /// Apply the initial protections on lower-vtl memory.
    ///
    /// After initialization, the default protections should be applied.
    pub fn apply_initial_lower_vtl_protections(
        &self,
        range: MemoryRange,
    ) -> Result<(), ApplyVtlProtectionsError> {
        self.apply_protections_from_flags(range, GuestVtl::Vtl0, HV_MAP_GPA_PERMISSIONS_ALL)
    }

    /// Query the current permissions for a vtl on a page.
    fn vtl_permissions(
        &self,
        vtl: Vtl,
        gpa: u64,
    ) -> Result<GpaVtlPermissions, QueryVtlPermissionsError> {
        match self.isolation {
            IsolationType::None | IsolationType::Vbs => unimplemented!(),
            IsolationType::Snp => {
                // TODO CVM GUEST VSM: track the permissions directly in
                // underhill. For now, use rmpquery--but note this is only
                // supported on Genoa+.
                let rmpadjust = self
                    .mshv_vtl
                    .rmpquery_page(
                        gpa,
                        vtl.try_into()
                            .expect("only query non-VTL 2 permissions on hardware cvm"),
                    )
                    .map_err(QueryVtlPermissionsError::Snp)?;

                Ok(GpaVtlPermissions::Snp(rmpadjust))
            }
            IsolationType::Tdx => todo!(),
        }
    }

    fn apply_protections_from_flags(
        &self,
        range: MemoryRange,
        vtl: GuestVtl,
        flags: HvMapGpaFlags,
    ) -> Result<(), ApplyVtlProtectionsError> {
        let permissions = GpaVtlPermissions::new(self.isolation, vtl, flags);
        self.apply_protections(range, vtl.into(), permissions)
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

/// An implementation of [`ProtectIsolatedMemory`] for Underhill VMs.
pub struct HardwareIsolatedMemoryProtector {
    // Serves as a lock for synchronizing visibility and page-protection changes.
    inner: Mutex<HardwareIsolatedMemoryProtectorInner>,
    layout: MemoryLayout,
    acceptor: Arc<MemoryAcceptor>,
    hypercall_overlay: VtlArray<Arc<Mutex<Option<HypercallOverlay>>>, 2>,
}

struct HypercallOverlay {
    gpn: u64,
    permissions: GpaVtlPermissions,
}

struct HardwareIsolatedMemoryProtectorInner {
    valid_encrypted: Arc<GuestValidMemory>,
    valid_shared: Arc<GuestValidMemory>,
    encrypted: Arc<GuestMemoryMapping>,
    default_vtl_permissions: DefaultVtlPermissions,
    vtl1_protections_enabled: bool,
}

impl HardwareIsolatedMemoryProtector {
    /// Returns a new instance.
    ///
    /// `shared` provides the mapping for shared memory. `vtl0` provides the
    /// mapping for encrypted memory.
    pub fn new(
        valid_encrypted: Arc<GuestValidMemory>,
        valid_shared: Arc<GuestValidMemory>,
        encrypted: Arc<GuestMemoryMapping>,
        layout: MemoryLayout,
        acceptor: Arc<MemoryAcceptor>,
    ) -> Self {
        Self {
            inner: Mutex::new(HardwareIsolatedMemoryProtectorInner {
                valid_encrypted,
                valid_shared,
                encrypted,
                // Grant only VTL 0 all permissions. This will be altered
                // later by VTL 1 enablement and by VTL 1 itself.
                default_vtl_permissions: DefaultVtlPermissions {
                    vtl0: HV_MAP_GPA_PERMISSIONS_ALL,
                    vtl1: None,
                },
                vtl1_protections_enabled: false,
            }),
            layout,
            acceptor,
            hypercall_overlay: VtlArray::from_fn(|_| Arc::new(Mutex::new(None))),
        }
    }

    fn apply_protections_with_overlay_handling(
        &self,
        vtl: GuestVtl,
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
                    let sub_range = MemoryRange::new((overlay.gpn + 1) * HV_PAGE_SIZE..right.end());
                    if !sub_range.is_empty() {
                        self.acceptor
                            .apply_protections_from_flags(sub_range, vtl, protections)?;
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
        vtl: GuestVtl,
        overlay: &HypercallOverlay,
    ) -> Result<(), ApplyVtlProtectionsError> {
        let range = MemoryRange::new(overlay.gpn * HV_PAGE_SIZE..(overlay.gpn + 1) * HV_PAGE_SIZE);

        self.acceptor
            .apply_protections(range, vtl.into(), overlay.permissions)?;

        Ok(())
    }
}

impl ProtectIsolatedMemory for HardwareIsolatedMemoryProtector {
    fn change_host_visibility(
        &self,
        shared: bool,
        gpns: &[u64],
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), (HvError, usize)> {
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
            .filter(|&gpn| inner.valid_shared.check_valid(gpn) != shared)
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
            &inner.valid_encrypted
        } else {
            &inner.valid_shared
        };
        for &range in &ranges {
            clear_bitmap.update_valid(range, false);
        }

        // There may be other threads concurrently accessing these pages. We
        // cannot change the page visibility state until these threads have
        // stopped those accesses. Flush the RCU domain that `guestmem` uses in
        // order to flush any threads accessing the pages. After this, we are
        // guaranteed no threads are accessing these pages (unless the pages are
        // also locked), since no bitmap currently allows access.
        guestmem::rcu().synchronize_blocking();

        if let IsolationType::Snp = self.acceptor.isolation {
            // We need to ensure that the guest TLB has been fully flushed since
            // the unaccept operation is not guaranteed to do so in hardware,
            // and the hypervisor is also not trusted with TLB hygiene.
            tlb_access.flush_entire();
        }

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

        let (result, ranges) = match self.acceptor.modify_gpa_visibility(host_visibility, &gpns) {
            Ok(()) => {
                // All gpns succeeded, so the whole set of ranges should be
                // processed.
                (Ok(()), ranges)
            }
            Err(err) => {
                if shared {
                    // A transition from private to shared should always
                    // succeed. There is no safe rollback path, so we must
                    // panic.
                    panic!(
                        "the hypervisor refused to transition pages to shared, we cannot safely roll back: {:?}",
                        err
                    );
                }

                // Only some ranges succeeded. Recreate ranges based on which
                // gpns succeeded, for further processing.
                let (successful_gpns, failed_gpns) = gpns.split_at(err.processed);
                let ranges = PagedRange::new(
                    0,
                    successful_gpns.len() * PagedRange::PAGE_SIZE,
                    successful_gpns,
                )
                .unwrap()
                .ranges()
                .map(|r| r.map(|r| MemoryRange::new(r.start..r.end)))
                .collect::<Result<Vec<_>, _>>()
                .expect("previous gpns was already checked");

                // Roll back the cleared bitmap for failed gpns, as they should
                // be still in their original state of shared.
                let rollback_ranges =
                    PagedRange::new(0, failed_gpns.len() * PagedRange::PAGE_SIZE, failed_gpns)
                        .unwrap()
                        .ranges()
                        .map(|r| r.map(|r| MemoryRange::new(r.start..r.end)))
                        .collect::<Result<Vec<_>, _>>()
                        .expect("previous gpns was already checked");

                for &range in &rollback_ranges {
                    clear_bitmap.update_valid(range, true);
                }

                // Figure out the index of the gpn that failed, in the
                // pre-filtered list that will be reported back to the caller.
                let failed_index = orig_gpns
                    .iter()
                    .position(|gpn| *gpn == failed_gpns[0])
                    .expect("failed gpn should be present in the list");

                (Err((err.source, failed_index)), ranges)
            }
        };

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
                    inner.encrypted.zero_range(range).expect("VTL 2 should have access to lower VTL memory and the page should be accepted");
                }
            }
        }

        // Allow accesses via the correct address.
        let set_bitmap = if shared {
            &inner.valid_shared
        } else {
            &inner.valid_encrypted
        };
        for &range in &ranges {
            set_bitmap.update_valid(range, true);
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
                        GuestVtl::Vtl0,
                        inner.default_vtl_permissions.vtl0,
                    )
                    .expect("should be able to apply default protections");

                if let Some(vtl1_protections) = inner.default_vtl_permissions.vtl1 {
                    self.acceptor
                        .apply_protections_from_flags(range, GuestVtl::Vtl1, vtl1_protections)
                        .expect(
                            "everything should be in a state where we can apply VTL protections",
                        );
                }
            }
        }

        // Return the original result of the underlying page visibility
        // transition call to the caller.
        result
    }

    fn query_host_visibility(
        &self,
        gpns: &[u64],
        host_visibility: &mut [HostVisibilityType],
    ) -> Result<(), (HvError, usize)> {
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
            *host_vis = if inner.valid_shared.check_valid(*gpn) {
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
        tlb_access: &mut dyn TlbFlushLockAccess,
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

            for gpn in
                ram_range.range.start() / PAGE_SIZE as u64..ram_range.range.end() / PAGE_SIZE as u64
            {
                // TODO GUEST_VSM: for now, use the encrypted mapping to
                // find all accepted memory. When lazy acceptance exists,
                // this should track all pages that have been accepted and
                // should be used instead.
                if !inner.valid_encrypted.check_valid(gpn) {
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

        // Invalidate the entire VTL 0 TLB to ensure that the new permissions
        // are observed.
        tlb_access.flush(GuestVtl::Vtl0);

        Ok(())
    }

    fn change_vtl_protections(
        &self,
        vtl: GuestVtl,
        gpns: &[u64],
        protections: HvMapGpaFlags,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), (HvError, usize)> {
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
        if gpns.iter().any(|&gpn| inner.valid_shared.check_valid(gpn)) {
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

        // Since page protections were modified, we must invalidate the entire
        // VTL 0 TLB to ensure that the new permissions are observed, and wait for
        // other CPUs to release all guest mappings before declaring that the VTL
        // protection change has completed.
        tlb_access.flush(GuestVtl::Vtl0);
        tlb_access.set_wait_for_tlb_locks(vtl);

        Ok(())
    }

    fn change_hypercall_overlay(
        &self,
        vtl: GuestVtl,
        gpn: u64,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) {
        // Should already have written contents to the page via the guest
        // memory object, confirming that this is a guest page
        assert!(
            self.layout
                .ram()
                .iter()
                .any(|r| r.range.contains_addr(gpn * HV_PAGE_SIZE))
        );

        let inner = self.inner.lock();

        let mut overlay = self.hypercall_overlay[vtl].lock();

        // Restore permissions on the previous overlay
        if let Some(overlay) = overlay.as_ref() {
            self.restore_overlay_permissions(vtl, overlay)
                .expect("applying vtl protections should succeed");
        }

        let current_permissions = match self.acceptor.isolation {
            IsolationType::None | IsolationType::Vbs => unreachable!(),
            IsolationType::Snp => {
                if inner.vtl1_protections_enabled {
                    // Safe to assume that rmpquery is available because
                    // guest vsm is only allowed if rmpquery is
                    self.acceptor
                        .vtl_permissions(vtl.into(), gpn * HV_PAGE_SIZE)
                        .expect("able to query vtl protections")
                } else {
                    // Since there's no VTL 1 and VTL 0 can't change its own
                    // permissions, the permissions should be the same as
                    // when VTL 2 initialized guest memory.
                    GpaVtlPermissions::new(IsolationType::Snp, vtl, HV_MAP_GPA_PERMISSIONS_ALL)
                }
            }
            IsolationType::Tdx => {
                // TODO TDX GUEST VSM: implement acceptor.vtl_permissions
                // For now, since guest vsm isn't enabled (therefore no VTL
                // 1), and VTL 0 can't change its own permissions, the
                // permissions should be the same as when VTL 2 initialized
                // guest memory.

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
                HV_MAP_GPA_PERMISSIONS_ALL.with_writable(false),
            )
            .expect("applying vtl protections should succeed");

        // Flush the guest TLB to ensure that the new permissions are observed.
        tlb_access.flush(vtl);
    }

    fn disable_hypercall_overlay(&self, vtl: GuestVtl, tlb_access: &mut dyn TlbFlushLockAccess) {
        let _lock = self.inner.lock();

        let mut overlay = self.hypercall_overlay[vtl].lock();

        if let Some(overlay) = overlay.as_ref() {
            self.restore_overlay_permissions(vtl, overlay)
                .expect("applying vtl protections should succeed");
        }

        *overlay = None;

        tlb_access.flush(vtl);
    }

    fn set_vtl1_protections_enabled(&self) {
        self.inner.lock().vtl1_protections_enabled = true;
    }

    fn vtl1_protections_enabled(&self) -> bool {
        self.inner.lock().vtl1_protections_enabled
    }
}
