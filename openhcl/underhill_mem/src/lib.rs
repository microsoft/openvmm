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
use guestmem::GuestMemoryBackingError;
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
use hvdef::HV_MAP_GPA_PERMISSIONS_NONE;
use hvdef::HV_PAGE_SHIFT;
use hvdef::HV_PAGE_SIZE;
use hvdef::HvError;
use hvdef::HvMapGpaFlags;
use hvdef::HypercallCode;
use hvdef::hypercall::AcceptMemoryType;
use hvdef::hypercall::HostVisibilityType;
use hvdef::hypercall::HvInputVtl;
use mapping::GuestMemoryMapping;
use mapping::GuestValidMemory;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use parking_lot::MutexGuard;
use registrar::RegisterMemory;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use thiserror::Error;
use virt::IsolationType;
use virt_mshv_vtl::GpnSource;
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
    Tdx(TdgMemPageGpaAttr, TdgMemPageAttrWriteR8),
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
                let mut vtl_permissions =
                    GpaVtlPermissions::Tdx(TdgMemPageGpaAttr::new(), TdgMemPageAttrWriteR8::new());
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
            GpaVtlPermissions::Tdx(attributes, mask) => {
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

    /// Accept pages for lower VTLs.
    pub fn accept_lower_vtl_pages(&self, range: MemoryRange) -> Result<(), AcceptPagesError> {
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

    fn unaccept_lower_vtl_pages(&self, range: MemoryRange) {
        match self.isolation {
            IsolationType::None => unreachable!(),
            IsolationType::Vbs => {
                // TODO VBS: is there something to do here?
            }
            IsolationType::Snp => {
                // Revoke permissions before unaccepting pages. This is required
                // because a subsequent page acceptance is not guaranteed to
                // reset permissions unless the hypervisor executed RMPUPDATE,
                // which it cannot be trusted to do. We set new permissions
                // ourselves, but that still leaves open a tiny window where the
                // guest could access the pages with the old permissions.
                for lower_vtl in [GuestVtl::Vtl0, GuestVtl::Vtl1] {
                    self.apply_protections(range, lower_vtl, HV_MAP_GPA_PERMISSIONS_NONE)
                        .unwrap();
                }
                self.mshv_vtl.pvalidate_pages(range, false, false).unwrap()
            }

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
        self.apply_protections(range, GuestVtl::Vtl0, HV_MAP_GPA_PERMISSIONS_ALL)
    }

    fn apply_protections(
        &self,
        range: MemoryRange,
        vtl: GuestVtl,
        flags: HvMapGpaFlags,
    ) -> Result<(), ApplyVtlProtectionsError> {
        let permissions = GpaVtlPermissions::new(self.isolation, vtl, flags);

        match permissions {
            GpaVtlPermissions::Vbs(flags) => {
                // For VBS-isolated VMs, the permissions apply to all lower
                // VTLs. Therefore VTL 0 cannot set its own permissions.
                assert_ne!(vtl, GuestVtl::Vtl0);

                self.mshv_hvcall
                    .modify_vtl_protection_mask(range, flags, HvInputVtl::from(vtl))
            }
            GpaVtlPermissions::Snp(rmpadjust) => {
                // For SNP VMs, the permissions apply to the specified VTL.
                // Therefore VTL 2 cannot specify its own permissions.
                self.mshv_vtl
                    .rmpadjust_pages(range, rmpadjust, false)
                    .map_err(|err| ApplyVtlProtectionsError::Snp {
                        failed_operation: err,
                        range,
                        permissions: rmpadjust,
                        vtl: vtl.into(),
                    })
            }
            GpaVtlPermissions::Tdx(attributes, mask) => {
                // For TDX VMs, the permissions apply to the specified VTL.
                // Therefore VTL 2 cannot specify its own permissions.
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
    vtl0: Arc<GuestMemoryMapping>,
    vtl1_protections_enabled: AtomicBool,
}

struct HardwareIsolatedMemoryProtectorInner {
    valid_encrypted: Arc<GuestValidMemory>,
    valid_shared: Arc<GuestValidMemory>,
    encrypted: Arc<GuestMemoryMapping>,
    default_vtl_permissions: DefaultVtlPermissions,
    overlay_pages: VtlArray<Vec<OverlayPage>, 2>,
    locked_pages: VtlArray<Vec<Box<[u64]>>, 2>,
}

struct OverlayPage {
    gpn: u64,
    previous_permissions: HvMapGpaFlags,
    overlay_permissions: HvMapGpaFlags,
    ref_count: u16,
    gpn_source: GpnSource,
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
        vtl0: Arc<GuestMemoryMapping>,
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
                overlay_pages: VtlArray::from_fn(|_| Vec::new()),
                locked_pages: VtlArray::from_fn(|_| Vec::new()),
            }),
            layout,
            acceptor,
            vtl0,
            vtl1_protections_enabled: AtomicBool::new(false),
        }
    }

    fn apply_protections_with_overlay_handling(
        &self,
        range: MemoryRange,
        target_vtl: GuestVtl,
        protections: HvMapGpaFlags,
        inner: &mut MutexGuard<'_, HardwareIsolatedMemoryProtectorInner>,
    ) -> Result<(), ApplyVtlProtectionsError> {
        let mut range_queue = VecDeque::new();
        range_queue.push_back(range);

        'outer: while let Some(range) = range_queue.pop_front() {
            for overlay_page in inner.overlay_pages[target_vtl].iter_mut() {
                let overlay_addr = overlay_page.gpn * HV_PAGE_SIZE;
                if range.contains_addr(overlay_addr) {
                    // If the overlay page is within the range, update the
                    // permissions that will be restored when it is unlocked.
                    overlay_page.previous_permissions = protections;
                    // And split the range around it.
                    let (left, right_with_overlay) =
                        range.split_at_offset(range.offset_of(overlay_addr).unwrap());
                    let (overlay, right) = right_with_overlay.split_at_offset(HV_PAGE_SIZE);
                    debug_assert_eq!(overlay.start_4k_gpn(), overlay_page.gpn);
                    debug_assert_eq!(overlay.len(), HV_PAGE_SIZE);
                    if !left.is_empty() {
                        range_queue.push_back(left);
                    }
                    if !right.is_empty() {
                        range_queue.push_back(right);
                    }
                    continue 'outer;
                }
            }
            // We can only reach here if the range does not contain any overlay
            // pages, so now we can apply the protections to the range.
            self.apply_protections(range, target_vtl, protections, GpnSource::GuestMemory)?
        }

        Ok(())
    }

    fn apply_protections(
        &self,
        range: MemoryRange,
        target_vtl: GuestVtl,
        protections: HvMapGpaFlags,
        gpn_source: GpnSource,
    ) -> Result<(), ApplyVtlProtectionsError> {
        if gpn_source == GpnSource::GuestMemory && target_vtl == GuestVtl::Vtl0 {
            // Only permissions imposed on VTL 0 guest memory are explicitly tracked
            self.vtl0.update_permission_bitmaps(range, protections);
        }
        self.acceptor
            .apply_protections(range, target_vtl, protections)
    }

    /// Get the permissions that the given VTL has to the given GPN.
    ///
    /// This function does not check for any protections applied by VTL 2,
    /// only those applied by lower VTLs.
    fn query_lower_vtl_permissions(
        &self,
        vtl: GuestVtl,
        gpn: u64,
    ) -> Result<HvMapGpaFlags, HvError> {
        if !self.is_in_guest_memory(gpn) {
            return Err(HvError::OperationDenied);
        }

        let res = match vtl {
            GuestVtl::Vtl0 => self
                .vtl0
                .query_access_permission(gpn)
                .unwrap_or(HV_MAP_GPA_PERMISSIONS_ALL),
            GuestVtl::Vtl1 => HV_MAP_GPA_PERMISSIONS_ALL,
        };

        Ok(res)
    }

    fn check_gpn_not_locked(
        &self,
        inner: &MutexGuard<'_, HardwareIsolatedMemoryProtectorInner>,
        vtl: GuestVtl,
        gpn: u64,
    ) -> Result<(), HvError> {
        // Overlay pages have special handling, being locked does not prevent that.
        // TODO: When uh_mem implements the returning of overlay pages, rather than
        // requiring them to also be locked through guestmem, the check for overlay
        // pages can be removed, as locked and overlay pages will be mutually exclusive.
        if inner.locked_pages[vtl].iter().flatten().any(|x| *x == gpn)
            && !inner.overlay_pages[vtl].iter().any(|p| p.gpn == gpn)
        {
            return Err(HvError::OperationDenied);
        }
        Ok(())
    }

    /// Checks whether the given GPN is present in guest RAM.
    fn is_in_guest_memory(&self, gpn: u64) -> bool {
        let gpa = gpn << HV_PAGE_SHIFT;
        self.layout.ram().iter().any(|r| r.range.contains_addr(gpa))
    }
}

impl ProtectIsolatedMemory for HardwareIsolatedMemoryProtector {
    fn change_host_visibility(
        &self,
        vtl: GuestVtl,
        shared: bool,
        gpns: &[u64],
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), (HvError, usize)> {
        let inner = self.inner.lock();

        for &gpn in gpns {
            // Validate the ranges are RAM.
            if !self.is_in_guest_memory(gpn) {
                return Err((HvError::OperationDenied, 0));
            }

            // Validate they're not locked.
            self.check_gpn_not_locked(&inner, vtl, gpn)
                .map_err(|x| (x, 0))?;

            // Don't allow overlay pages to be shared.
            if shared && inner.overlay_pages[vtl].iter().any(|p| p.gpn == gpn) {
                return Err((HvError::OperationDenied, 0));
            }
        }

        // Filter out the GPNs that are already in the correct state. If the
        // page is becoming shared, make sure the requesting VTL has read/write
        // vtl permissions to the page.
        let orig_gpns = gpns;
        let mut failed_vtl_permission_index = None;
        let gpns = gpns
            .iter()
            .copied()
            .enumerate()
            .take_while(|&(index, gpn)| {
                if vtl == GuestVtl::Vtl0 && shared && self.vtl1_protections_enabled() {
                    let permissions = self
                        .vtl0
                        .query_access_permission(gpn)
                        .expect("vtl 1 protections enabled, vtl permissions should be tracked");
                    if !permissions.readable() || !permissions.writable() {
                        failed_vtl_permission_index = Some(index);
                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            })
            .filter_map(|(_, gpn)| {
                if inner.valid_shared.check_valid(gpn) != shared {
                    Some(gpn)
                } else {
                    None
                }
            })
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
            if shared && vtl == GuestVtl::Vtl0 {
                // Accessing these pages through the encrypted mapping is now
                // invalid. Make sure the VTL bitmaps reflect this. We could
                // call apply_protections here but that would result in an extra
                // hardware interaction that we don't need since we're about to
                // unaccept the pages anyways.
                self.vtl0
                    .update_permission_bitmaps(range, HV_MAP_GPA_PERMISSIONS_NONE);
            }

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
                self.acceptor.unaccept_lower_vtl_pages(range);
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
                (
                    match failed_vtl_permission_index {
                        Some(index) => Err((HvError::AccessDenied, index)),
                        None => Ok(()),
                    },
                    ranges,
                )
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
                    .accept_lower_vtl_pages(range)
                    .expect("everything should be in a state where we can accept VTL0 pages");

                // For SNP, zero the memory before allowing the guest to access
                // them. For TDX, this is done by the TDX module. For mshv, this is
                // done by the hypervisor.
                if self.acceptor.isolation == IsolationType::Snp {
                    inner.encrypted.zero_range(range).expect("VTL 2 should have access to lower VTL memory, the page should be accepted, there should be no vtl protections yet.")
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
            // Apply vtl protections so that the guest can use them. Any
            // overlay pages won't be host visible, so just apply the default
            // protections directly without handling them.
            for &range in &ranges {
                // Make sure we reset the permissions bitmaps for VTL 0.
                self.apply_protections(
                    range,
                    GuestVtl::Vtl0,
                    inner.default_vtl_permissions.vtl0,
                    GpnSource::GuestMemory,
                )
                .expect("should be able to apply default protections");

                if let Some(vtl1_protections) = inner.default_vtl_permissions.vtl1 {
                    self.apply_protections(
                        range,
                        GuestVtl::Vtl1,
                        vtl1_protections,
                        GpnSource::GuestMemory,
                    )
                    .expect("everything should be in a state where we can apply VTL protections");
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
            if !self.is_in_guest_memory(gpn) {
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
        target_vtl: GuestVtl,
        vtl_protections: HvMapGpaFlags,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), HvError> {
        // Prevent visibility changes while VTL protections are being
        // applied.
        //
        // TODO: This does not need to be synchronized against other
        // threads performing VTL protection changes; whichever thread
        // finishes last will control the outcome.
        let mut inner = self.inner.lock();

        inner
            .default_vtl_permissions
            .set(target_vtl, vtl_protections);

        let mut ranges = Vec::new();
        for ram_range in self.layout.ram().iter() {
            let mut protect_start = ram_range.range.start();
            let mut page_count = 0;

            for gpn in
                ram_range.range.start() / PAGE_SIZE as u64..ram_range.range.end() / PAGE_SIZE as u64
            {
                // TODO GUEST VSM: for now, use the encrypted mapping to
                // find all accepted memory. When lazy acceptance exists,
                // this should track all pages that have been accepted and
                // should be used instead.
                // Also don't attempt to change the permissions of locked pages.
                if inner.valid_encrypted.check_valid(gpn) {
                    self.check_gpn_not_locked(&inner, target_vtl, gpn)?;
                    page_count += 1;
                } else {
                    if page_count > 0 {
                        let end_address = protect_start + (page_count * PAGE_SIZE as u64);
                        ranges.push(MemoryRange::new(protect_start..end_address));
                    }
                    protect_start = (gpn + 1) * PAGE_SIZE as u64;
                    page_count = 0;
                }
            }

            if page_count > 0 {
                let end_address = protect_start + (page_count * PAGE_SIZE as u64);
                ranges.push(MemoryRange::new(protect_start..end_address));
            }
        }

        for range in ranges {
            self.apply_protections_with_overlay_handling(
                range,
                target_vtl,
                vtl_protections,
                &mut inner,
            )
            .unwrap();
        }

        // Flush any threads accessing pages that had their VTL protections
        // changed.
        guestmem::rcu().synchronize_blocking();

        // Invalidate the entire VTL 0 TLB to ensure that the new permissions
        // are observed.
        tlb_access.flush(GuestVtl::Vtl0);
        tlb_access.set_wait_for_tlb_locks(target_vtl);

        Ok(())
    }

    fn change_vtl_protections(
        &self,
        target_vtl: GuestVtl,
        gpns: &[u64],
        protections: HvMapGpaFlags,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), (HvError, usize)> {
        // Prevent visibility changes while VTL protections are being
        // applied. This does not need to be synchronized against other
        // threads performing VTL protection changes; whichever thread
        // finishes last will control the outcome.
        let inner = self.inner.lock();

        // Validate the ranges are RAM.
        for &gpn in gpns {
            if !self.is_in_guest_memory(gpn) {
                return Err((HvError::OperationDenied, 0));
            }

            // Validate they're not locked.
            self.check_gpn_not_locked(&inner, target_vtl, gpn)
                .map_err(|x| (x, 0))?;

            // Validate they're not overlay pages.
            if inner.overlay_pages[target_vtl].iter().any(|p| p.gpn == gpn) {
                return Err((HvError::OperationDenied, 0));
            }
        }

        // Protections cannot be applied to a host-visible page
        if gpns.iter().any(|&gpn| inner.valid_shared.check_valid(gpn)) {
            return Err((HvError::OperationDenied, 0));
        }

        let ranges = PagedRange::new(0, gpns.len() * PagedRange::PAGE_SIZE, gpns)
            .unwrap()
            .ranges()
            .map(|r| r.map(|r| MemoryRange::new(r.start..r.end)))
            .collect::<Result<Vec<_>, _>>()
            .unwrap(); // Ok to unwrap, we've validated the gpns above.

        for range in ranges {
            self.apply_protections(range, target_vtl, protections, GpnSource::GuestMemory)
                .unwrap();
        }

        // Flush any threads accessing pages that had their VTL protections
        // changed.
        guestmem::rcu().synchronize_blocking();

        // Since page protections were modified, we must invalidate the entire
        // VTL 0 TLB to ensure that the new permissions are observed, and wait for
        // other CPUs to release all guest mappings before declaring that the VTL
        // protection change has completed.
        tlb_access.flush(GuestVtl::Vtl0);
        tlb_access.set_wait_for_tlb_locks(target_vtl);

        Ok(())
    }

    fn register_overlay_page(
        &self,
        vtl: GuestVtl,
        gpn: u64,
        gpn_source: GpnSource,
        check_perms: HvMapGpaFlags,
        new_perms: Option<HvMapGpaFlags>,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), HvError> {
        let mut inner = self.inner.lock();

        // If the page is already registered as an overlay page, just check
        // the permissions are adequate. If the permissions requested are
        // different from the ones already registered just do best effort,
        // there is no spec-guarantee of which one "wins".
        if let Some(registered) = inner.overlay_pages[vtl].iter_mut().find(|p| p.gpn == gpn) {
            let needed_perms = new_perms.unwrap_or(check_perms);
            if registered.overlay_permissions.into_bits() | needed_perms.into_bits()
                != registered.overlay_permissions.into_bits()
            {
                return Err(HvError::OperationDenied);
            }
            registered.ref_count += 1;
            return Ok(());
        }

        let current_perms = match gpn_source {
            GpnSource::GuestMemory => {
                // Check that the required permissions are present.
                let current_perms = self.query_lower_vtl_permissions(vtl, gpn)?;
                if current_perms.into_bits() | check_perms.into_bits() != current_perms.into_bits()
                {
                    return Err(HvError::OperationDenied);
                }

                // Protections cannot be applied to a host-visible page.
                if inner.valid_shared.check_valid(gpn) {
                    return Err(HvError::OperationDenied);
                }

                current_perms
            }
            GpnSource::Dma => {
                if self.is_in_guest_memory(gpn) {
                    // DMA memory must not be in guest RAM.
                    return Err(HvError::OperationDenied);
                }

                HV_MAP_GPA_PERMISSIONS_NONE
            }
        };

        // Or a locked page.
        self.check_gpn_not_locked(&inner, vtl, gpn)?;

        // Everything's validated, change the permissions.
        if let Some(new_perms) = new_perms {
            self.apply_protections(
                MemoryRange::from_4k_gpn_range(gpn..gpn + 1),
                vtl,
                new_perms,
                gpn_source,
            )
            .map_err(|_| HvError::OperationDenied)?;
        }

        // Nothing from this point on can fail, so we can safely register the overlay page.
        inner.overlay_pages[vtl].push(OverlayPage {
            gpn,
            previous_permissions: current_perms,
            overlay_permissions: new_perms.unwrap_or(current_perms),
            ref_count: 1,
            gpn_source,
        });

        // Flush any threads accessing pages that had their VTL protections
        // changed.
        guestmem::rcu().synchronize_blocking();

        // Since page protections were modified, we must invalidate the TLB to
        // ensure that the new permissions are observed, and wait for other CPUs
        // to release all guest mappings before declaring that the VTL
        // protection change has completed.
        tlb_access.flush(vtl);
        tlb_access.set_wait_for_tlb_locks(vtl);

        Ok(())
    }

    fn unregister_overlay_page(
        &self,
        vtl: GuestVtl,
        gpn: u64,
        tlb_access: &mut dyn TlbFlushLockAccess,
    ) -> Result<(), HvError> {
        let mut inner = self.inner.lock();
        let overlay_pages = &mut inner.overlay_pages[vtl];

        // Find the overlay page.
        let index = overlay_pages
            .iter()
            .position(|p| p.gpn == gpn)
            .ok_or(HvError::OperationDenied)?;

        // If this overlay page has been registered multiple times, just
        // decrement the reference count and return. We don't implement
        // full handling of multiple registrations with different permissions,
        // since it's best effort anyways.
        if overlay_pages[index].ref_count > 1 {
            overlay_pages[index].ref_count -= 1;
            return Ok(());
        }

        // Restore its permissions.
        self.apply_protections(
            MemoryRange::from_4k_gpn_range(gpn..gpn + 1),
            vtl,
            overlay_pages[index].previous_permissions,
            overlay_pages[index].gpn_source,
        )
        .map_err(|_| HvError::OperationDenied)?;

        // Nothing from this point on can fail, so we can safely unregister the overlay page.
        overlay_pages.remove(index);

        // Flush any threads accessing pages that had their VTL protections
        // changed.
        guestmem::rcu().synchronize_blocking();

        // Since page protections were modified, we must invalidate the TLB to
        // ensure that the new permissions are observed, and wait for other CPUs
        // to release all guest mappings before declaring that the VTL
        // protection change has completed.
        tlb_access.flush(vtl);
        tlb_access.set_wait_for_tlb_locks(vtl);
        Ok(())
    }

    fn is_overlay_page(&self, vtl: GuestVtl, gpn: u64) -> bool {
        self.inner.lock().overlay_pages[vtl]
            .iter()
            .any(|p| p.gpn == gpn)
    }

    fn lock_gpns(&self, vtl: GuestVtl, gpns: &[u64]) -> Result<(), GuestMemoryBackingError> {
        // Locking a page multiple times is allowed, so no need to check
        // for duplicates.
        // We also need to allow locking overlay pages for now.
        // TODO: We probably don't want to allow locking overlay pages once
        // we return the pointer for them instead of going through guestmem::lock.
        // TODO: other preconditions?
        self.inner.lock().locked_pages[vtl].push(gpns.to_vec().into_boxed_slice());
        Ok(())
    }

    fn unlock_gpns(&self, vtl: GuestVtl, gpns: &[u64]) {
        let mut inner = self.inner.lock();
        let locked_pages = &mut inner.locked_pages[vtl];
        for (i, w) in locked_pages.iter().enumerate() {
            if **w == *gpns {
                locked_pages.swap_remove(i);
                return;
            }
        }

        // Don't change protections on locked pages to avoid conflicting
        // with unregister_overlay_page.
        // TODO: Is this the right decision even after we separate overlay and
        // locked pages?

        panic!("Tried to unlock pages that were not locked");
    }

    fn set_vtl1_protections_enabled(&self) {
        self.vtl1_protections_enabled
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn vtl1_protections_enabled(&self) -> bool {
        self.vtl1_protections_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}
