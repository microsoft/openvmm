// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM memory-slot and confidential guest backing management.
//!
//! Confidential RAM slots use userspace memory for shared access and a
//! guestmemfd for private access. This module records both sides of each slot,
//! selects the appropriate backing when a range is mapped, validates private
//! launch ranges, and discards stale contents when ownership changes.

#[cfg(guest_arch = "aarch64")]
use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
#[cfg(guest_arch = "aarch64")]
use crate::cca::map_cca_conversion_error;
use inspect::Inspect;
use memory_range::MemoryRange;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("kvm memory operation failed")]
    Kvm(#[from] kvm::Error),
    #[error("cannot resize KVM guest_memfd memory slot")]
    CannotResizeGuestMemfdSlot,
    #[error("private memory range is not contained in guest_memfd private memory")]
    InvalidPrivateMemoryRange,
    #[error("invalid KVM_HC_MAP_GPA_RANGE request")]
    InvalidMapGpaRange,
    #[error("unsupported KVM_HC_MAP_GPA_RANGE attributes: {0:#x}")]
    UnsupportedMapGpaRangeAttributes(u64),
    #[error("failed to discard shared backing after private conversion")]
    DiscardSharedBacking(#[source] std::io::Error),
    #[error("failed to discard private backing after shared conversion")]
    DiscardPrivateBacking(#[source] std::io::Error),
    #[error("unsupported isolation configuration: {0}")]
    UnsupportedIsolationConfiguration(&'static str),
}

#[derive(Debug, Inspect)]
/// A registered KVM memory slot and its confidential-memory metadata.
pub(crate) struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
    guest_memfd_offset: Option<u64>,
    private_state: Option<KvmGuestMemfdPrivateState>,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
/// Slot-indexed memory mappings currently registered with KVM.
pub(crate) struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    pub(crate) ranges: Vec<Option<KvmMemoryRange>>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
/// A private guest range paired with the userspace source used for launch.
pub(crate) struct KvmPrivateMemoryRange {
    /// Guest-physical range covered by the private slot.
    pub(crate) gpa: MemoryRange,
    /// Userspace source address corresponding to the start of `gpa`.
    pub(crate) hva: *mut u8,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(crate) struct KvmMemoryRangeSegment {
    range: MemoryRange,
    host_addr: *mut u8,
    guest_memfd_offset: u64,
}

#[derive(Debug, Inspect)]
#[inspect(external_tag)]
/// Backing strategy for partition memory slots.
pub(crate) enum KvmMemoryBackingMode {
    /// Register only the caller-provided userspace mapping.
    Userspace,
    /// Register shared userspace and private guestmemfd backing for RAM.
    GuestMemfd(KvmGuestMemfdBacking),
}

#[derive(Debug, Inspect)]
/// Partition-owned guestmemfd and its packed mapping of guest RAM ranges.
pub(crate) struct KvmGuestMemfdBacking {
    #[inspect(skip)]
    file: File,
    #[inspect(iter_by_index)]
    ranges: Vec<KvmGuestMemfdRange>,
    private_state: KvmGuestMemfdPrivateState,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
pub(crate) enum KvmGuestMemfdPrivateState {
    #[cfg(guest_arch = "x86_64")]
    VmAttributes,
    #[cfg(guest_arch = "aarch64")]
    GuestMemfdDefault,
}

impl KvmGuestMemfdPrivateState {
    fn uses_vm_attributes(self) -> bool {
        #[cfg(guest_arch = "x86_64")]
        {
            matches!(self, Self::VmAttributes)
        }
        #[cfg(guest_arch = "aarch64")]
        {
            false
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
struct KvmGuestMemfdRange {
    range: MemoryRange,
    file_offset: u64,
}

#[derive(Debug)]
enum KvmMemoryBacking<'a> {
    Userspace,
    GuestMemfd {
        file: &'a File,
        file_offset: u64,
        private_state: KvmGuestMemfdPrivateState,
    },
}

impl KvmMemoryBackingMode {
    /// Creates one guestmemfd spanning the supplied RAM ranges.
    ///
    /// Guest ranges are packed contiguously into the file in iteration order.
    /// `private_state` describes how private state is established.
    pub(crate) fn guest_memfd(
        kvm: &kvm::Partition,
        ram_ranges: impl IntoIterator<Item = MemoryRange>,
        private_state: KvmGuestMemfdPrivateState,
    ) -> Result<Self, MemoryError> {
        check_private_memory_extensions(kvm, private_state)?;

        let mut file_size = 0u64;
        let mut ranges = Vec::new();
        for range in ram_ranges {
            ranges.push(KvmGuestMemfdRange {
                range,
                file_offset: file_size,
            });
            file_size += range.len();
        }

        Ok(Self::GuestMemfd(KvmGuestMemfdBacking {
            file: kvm.create_guest_memfd(file_size)?,
            ranges,
            private_state,
        }))
    }
}

impl KvmPartitionInner {
    /// # Safety
    ///
    /// `data..data+size` must be and remain an allocated VA range until the
    /// partition is destroyed or the region is unmapped.
    unsafe fn map_region(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        readonly: bool,
    ) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size as u64);
        let backing = self.memory_backing(range)?;
        let mut state = self.memory.lock();

        // Memory slots cannot be resized but can be moved within the guest
        // address space. Find the existing slot if there is one.
        let mut slot_to_use = None;
        for (slot, range) in state.ranges.iter_mut().enumerate() {
            match range {
                Some(range) if range.host_addr == data => {
                    slot_to_use = Some(slot);
                    break;
                }
                Some(_) => (),
                None => slot_to_use = Some(slot),
            }
        }
        if slot_to_use.is_none() {
            slot_to_use = Some(state.ranges.len());
            state.ranges.push(None);
        }
        let slot_to_use = slot_to_use.unwrap();
        if let Some(existing_range) = &state.ranges[slot_to_use] {
            if existing_range.guest_memfd_offset.is_some()
                && existing_range.range.len() != size as u64
            {
                return Err(MemoryError::CannotResizeGuestMemfdSlot.into());
            }
            if existing_range
                .private_state
                .is_some_and(KvmGuestMemfdPrivateState::uses_vm_attributes)
            {
                self.kvm.set_memory_attributes(
                    existing_range.range.start(),
                    existing_range.range.len(),
                    0,
                )?;
            }
            #[cfg(guest_arch = "aarch64")]
            if existing_range.private_state == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault) {
                let guest_memfd_offset = existing_range
                    .guest_memfd_offset
                    .ok_or(MemoryError::InvalidPrivateMemoryRange)?;
                if let Err(err) = self.discard_stale_private_memory_backing(
                    &[KvmMemoryRangeSegment {
                        range: existing_range.range,
                        host_addr: existing_range.host_addr,
                        guest_memfd_offset,
                    }],
                    false,
                    "CCA slot replacement",
                ) {
                    self.mark_cca_fatal();
                    return Err(err.into());
                }
            }
            if existing_range.guest_memfd_offset.is_some() {
                // SAFETY: clearing a slot removes the memory reference.
                if let Err(err) = unsafe { self.clear_slot(slot_to_use, true) } {
                    #[cfg(guest_arch = "aarch64")]
                    if existing_range.private_state
                        == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault)
                    {
                        self.mark_cca_fatal();
                    }
                    return Err(err.into());
                }
                state.ranges[slot_to_use] = None;
            }
        }
        let (guest_memfd_offset, private_state) = match backing {
            KvmMemoryBacking::Userspace => {
                // SAFETY: `map_region` requires its caller to keep
                // `data..data+size` valid until this guest-physical range is
                // unmapped or the partition is destroyed.
                unsafe {
                    self.kvm.set_user_memory_region(
                        slot_to_use as u32,
                        data,
                        size,
                        addr,
                        readonly,
                    )?
                };
                (None, None)
            }
            KvmMemoryBacking::GuestMemfd {
                file,
                file_offset,
                private_state,
            } => {
                // SAFETY: `map_region` requires its caller to keep
                // `data..data+size` valid until this guest-physical range is
                // unmapped or the partition is destroyed. The partition owns the
                // backing guestmemfd for at least as long as KVM references it.
                unsafe {
                    self.kvm.set_user_memory_region2(
                        slot_to_use as u32,
                        data,
                        size,
                        addr,
                        readonly,
                        Some((file, file_offset)),
                    )?;
                };
                if private_state.uses_vm_attributes() {
                    if let Err(err) = self.kvm.set_memory_attributes(
                        addr,
                        size as u64,
                        kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64,
                    ) {
                        // SAFETY: clearing a slot removes the memory reference.
                        unsafe { self.clear_slot(slot_to_use, true)? };
                        state.ranges[slot_to_use] = None;
                        return Err(err.into());
                    }
                }
                (Some(file_offset), Some(private_state))
            }
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range,
            guest_memfd_offset,
            private_state,
        });
        Ok(())
    }

    fn memory_backing(&self, range: MemoryRange) -> Result<KvmMemoryBacking<'_>, MemoryError> {
        match &self.memory_backing_mode {
            KvmMemoryBackingMode::Userspace => Ok(KvmMemoryBacking::Userspace),
            KvmMemoryBackingMode::GuestMemfd(backing) => {
                match classify_guest_memfd_backing(range, &backing.ranges)? {
                    Some(file_offset) => Ok(KvmMemoryBacking::GuestMemfd {
                        file: &backing.file,
                        file_offset,
                        private_state: backing.private_state,
                    }),
                    None => Ok(KvmMemoryBacking::Userspace),
                }
            }
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that clearing the target slot is valid.
    unsafe fn clear_slot(&self, slot: usize, guest_memfd_backed: bool) -> Result<(), kvm::Error> {
        if guest_memfd_backed {
            // SAFETY: the caller ensures clearing this slot is valid.
            unsafe {
                self.kvm.set_user_memory_region2(
                    slot as u32,
                    std::ptr::null_mut(),
                    0,
                    0,
                    false,
                    None,
                )
            }
        } else {
            // SAFETY: the caller ensures clearing this slot is valid.
            unsafe {
                self.kvm
                    .set_user_memory_region(slot as u32, std::ptr::null_mut(), 0, 0, false)
            }
        }
    }

    /// Applies a guest-requested SNP shared/private state change.
    ///
    /// `page_count` is always expressed in 4-KiB pages by
    /// `KVM_HC_MAP_GPA_RANGE`. The page-size bits in `map_attributes` describe
    /// the guest's preferred processing granularity, but do not change the
    /// units of `page_count`.
    ///
    /// The range must be non-empty, page-aligned, continuously covered by
    /// guestmemfd-backed slots, and request either the encrypted or decrypted
    /// state. After updating KVM's private-memory attributes, the backing for
    /// the old state is discarded so stale data cannot be reused if the page
    /// later transitions back.
    #[cfg(guest_arch = "x86_64")]
    pub(crate) fn set_map_gpa_range_attributes(
        &self,
        gpa: u64,
        page_count: u64,
        map_attributes: u64,
    ) -> Result<(), MemoryError> {
        const KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK: u64 = 0x3;
        const KVM_MAP_GPA_RANGE_ENC_STATUS_MASK: u64 = 0xf << 4;

        let size = page_count
            .checked_mul(hvdef::HV_PAGE_SIZE)
            .ok_or(MemoryError::InvalidMapGpaRange)?;
        let end = gpa
            .checked_add(size)
            .ok_or(MemoryError::InvalidMapGpaRange)?;
        if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE) || size == 0 {
            return Err(MemoryError::InvalidMapGpaRange);
        }
        let unsupported_attributes = map_attributes
            & !(KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK | KVM_MAP_GPA_RANGE_ENC_STATUS_MASK);
        if unsupported_attributes != 0 {
            return Err(MemoryError::UnsupportedMapGpaRangeAttributes(
                map_attributes,
            ));
        }
        let private = match map_attributes & KVM_MAP_GPA_RANGE_ENC_STATUS_MASK {
            kvm::KVM_MAP_GPA_RANGE_DECRYPTED_UAPI => false,
            kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI => true,
            _ => {
                return Err(MemoryError::UnsupportedMapGpaRangeAttributes(
                    map_attributes,
                ));
            }
        };

        let range = MemoryRange::new(gpa..end);
        let state = self.memory.lock();
        let segments = guest_memfd_range_segments(range, &state.ranges)?;

        let attributes = if private {
            kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64
        } else {
            0
        };
        tracing::debug!(
            gpa,
            size,
            page_count,
            map_attributes,
            private,
            "KVM_HC_MAP_GPA_RANGE set memory attributes"
        );
        self.kvm.set_memory_attributes(gpa, size, attributes)?;
        self.discard_stale_private_memory_backing(&segments, private, "SNP")?;
        Ok(())
    }

    /// Discards data from the backing that is no longer selected by KVM.
    ///
    /// Guestmemfd memory slots have separate shared userspace and private
    /// guestmemfd backings. For a shared-to-private conversion, discard the
    /// shared backing with `MADV_REMOVE` (falling back to `MADV_DONTNEED` for
    /// anonymous mappings). For a private-to-shared conversion, punch a hole in
    /// guestmemfd so private data cannot become visible after a later conversion
    /// back to private.
    pub(crate) fn discard_stale_private_memory_backing(
        &self,
        segments: &[KvmMemoryRangeSegment],
        private: bool,
        isolation_name: &'static str,
    ) -> Result<(), MemoryError> {
        if private {
            for segment in segments {
                tracing::debug!(
                    gpa = segment.range.start(),
                    size = segment.range.len(),
                    hva = segment.host_addr as usize,
                    isolation_name,
                    "discarding shared backing after private conversion"
                );
                let mut ret = unsafe {
                    libc::madvise(
                        segment.host_addr.cast(),
                        segment.range.len() as usize,
                        libc::MADV_REMOVE,
                    )
                };
                if ret != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
                {
                    // MADV_REMOVE requires a shared file-backed mapping.
                    ret = unsafe {
                        libc::madvise(
                            segment.host_addr.cast(),
                            segment.range.len() as usize,
                            libc::MADV_DONTNEED,
                        )
                    };
                }
                if ret != 0 {
                    return Err(MemoryError::DiscardSharedBacking(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
        } else {
            let KvmMemoryBackingMode::GuestMemfd(backing) = &self.memory_backing_mode else {
                return Err(MemoryError::InvalidMapGpaRange);
            };
            for segment in segments {
                tracing::debug!(
                    gpa = segment.range.start(),
                    size = segment.range.len(),
                    guest_memfd_offset = segment.guest_memfd_offset,
                    isolation_name,
                    "discarding private backing after shared conversion"
                );
                let ret = unsafe {
                    libc::fallocate(
                        backing.file.as_raw_fd(),
                        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                        segment.guest_memfd_offset as libc::off_t,
                        segment.range.len() as libc::off_t,
                    )
                };
                if ret != 0 {
                    return Err(MemoryError::DiscardPrivateBacking(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Applies a KVM CCA memory-fault/RIPAS state transition.
    ///
    /// The kernel supplies a page-aligned range and indicates whether it must
    /// become private. The range must remain within configured RAM. As with SNP
    /// conversions, the old backing is discarded after KVM accepts the new
    /// memory attribute so stale contents cannot reappear on a later transition.
    #[cfg(guest_arch = "aarch64")]
    pub(crate) fn handle_cca_ripas_change(
        &self,
        gpa: u64,
        size: u64,
        flags: u64,
    ) -> Result<(), KvmError> {
        let end = gpa
            .checked_add(size)
            .ok_or(KvmError::InvalidCcaMemoryFault)?;
        if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE)
            || !size.is_multiple_of(hvdef::HV_PAGE_SIZE)
            || size == 0
        {
            return Err(KvmError::InvalidCcaMemoryFault);
        }

        let unsupported_flags = flags & !kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI;
        if unsupported_flags != 0 {
            return Err(KvmError::UnsupportedCcaMemoryFaultFlags(flags));
        }

        let private = flags & kvm::KVM_MEMORY_EXIT_FLAG_PRIVATE_UAPI != 0;
        let range = MemoryRange::new(gpa..end);
        let state = self.memory.lock();
        let segments = guest_memfd_range_intersections(range, &state.ranges)
            .map_err(map_cca_conversion_error)?;

        tracing::debug!(gpa, size, flags, private, "KVM CCA RIPAS change");
        if let Err(err) = self.discard_stale_private_memory_backing(&segments, private, "CCA") {
            self.mark_cca_fatal();
            return Err(map_cca_conversion_error(err));
        }
        Ok(())
    }
}

#[cfg(any(guest_arch = "aarch64", test))]
fn guest_memfd_range_intersections(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<Vec<KvmMemoryRangeSegment>, MemoryError> {
    let mut segments = guest_memfd_intersections(range, slots);
    segments.sort_by_key(|segment| segment.range.start());
    if segments
        .windows(2)
        .any(|segments| segments[0].range.end() > segments[1].range.start())
    {
        return Err(MemoryError::InvalidMapGpaRange);
    }
    Ok(segments)
}

pub(crate) fn guest_memfd_range_segments(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<Vec<KvmMemoryRangeSegment>, MemoryError> {
    let mut segments = guest_memfd_intersections(range, slots);
    segments.sort_by_key(|segment| segment.range.start());

    let mut cursor = range.start();
    for segment in &segments {
        if segment.range.start() != cursor {
            return Err(MemoryError::InvalidMapGpaRange);
        }
        cursor = segment.range.end();
    }
    if cursor != range.end() {
        return Err(MemoryError::InvalidMapGpaRange);
    }

    Ok(segments)
}

fn guest_memfd_intersections(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Vec<KvmMemoryRangeSegment> {
    slots
        .iter()
        .flatten()
        .filter_map(|slot| {
            let guest_memfd_offset = slot.guest_memfd_offset?;
            let start = range.start().max(slot.range.start());
            let end = range.end().min(slot.range.end());
            (start < end).then(|| {
                let slot_offset = start - slot.range.start();
                KvmMemoryRangeSegment {
                    range: MemoryRange::new(start..end),
                    host_addr: slot.host_addr.wrapping_add(slot_offset as usize),
                    guest_memfd_offset: guest_memfd_offset + slot_offset,
                }
            })
        })
        .collect()
}

/// Resolves an imported range to a private guestmemfd slot and source HVA.
///
/// The entire range must be contained in one slot whose private attribute is
/// already active.
pub(crate) fn private_memory_range_from_slots(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<KvmPrivateMemoryRange, MemoryError> {
    let slot = slots
        .iter()
        .flatten()
        .find(|slot| slot.range.contains(&range))
        .ok_or(MemoryError::InvalidPrivateMemoryRange)?;

    if slot.guest_memfd_offset.is_none() || slot.private_state.is_none() {
        return Err(MemoryError::InvalidPrivateMemoryRange);
    }

    let offset = range.start() - slot.range.start();
    Ok(KvmPrivateMemoryRange {
        gpa: range,
        hva: slot.host_addr.wrapping_add(offset as usize),
    })
}

/// Verifies the KVM capabilities required for guestmemfd private memory.
pub(crate) fn check_private_memory_extensions(
    kvm: &kvm::Partition,
    private_state: KvmGuestMemfdPrivateState,
) -> Result<(), MemoryError> {
    require_kvm_extension(kvm, kvm::KVM_CAP_USER_MEMORY2, "KVM_CAP_USER_MEMORY2")?;
    require_kvm_extension(kvm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;
    let (capability, capability_name) = match private_state {
        #[cfg(guest_arch = "x86_64")]
        KvmGuestMemfdPrivateState::VmAttributes => {
            (kvm::KVM_CAP_MEMORY_ATTRIBUTES, "KVM_CAP_MEMORY_ATTRIBUTES")
        }
        #[cfg(guest_arch = "aarch64")]
        KvmGuestMemfdPrivateState::GuestMemfdDefault => (
            kvm::KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES_UAPI,
            "KVM_CAP_GUEST_MEMFD_MEMORY_ATTRIBUTES",
        ),
    };
    let memory_attributes = require_kvm_extension(kvm, capability, capability_name)?;
    if memory_attributes as u64 & kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64 == 0 {
        return Err(kvm::Error::MissingCapability(capability_name).into());
    }
    Ok(())
}

fn require_kvm_extension(
    kvm: &kvm::Partition,
    extension: u32,
    capability: &'static str,
) -> Result<i32, MemoryError> {
    let value = kvm
        .check_extension(extension)
        .map_err(kvm::Error::CheckExtension)?;
    if value == 0 {
        return Err(kvm::Error::MissingCapability(capability).into());
    }
    Ok(value)
}

fn classify_guest_memfd_backing(
    range: MemoryRange,
    ram_ranges: &[KvmGuestMemfdRange],
) -> Result<Option<u64>, MemoryError> {
    let mut containing_ranges = ram_ranges
        .iter()
        .filter(|ram_range| ram_range.range.contains(&range));
    if let Some(ram_range) = containing_ranges.next() {
        if containing_ranges.next().is_some() {
            return Err(MemoryError::UnsupportedIsolationConfiguration(
                "KVM guest_memfd mappings must be contained in exactly one RAM range",
            ));
        }
        return Ok(Some(
            ram_range.file_offset + (range.start() - ram_range.range.start()),
        ));
    }

    if ram_ranges
        .iter()
        .any(|ram_range| ram_range.range.overlaps(&range))
    {
        return Err(MemoryError::UnsupportedIsolationConfiguration(
            "KVM guest_memfd mappings must be fully contained in one RAM range",
        ));
    }

    Ok(None)
}

impl virt::PartitionMemoryMapper for KvmPartition {
    fn memory_mapper(&self, vtl: hvdef::Vtl) -> Arc<dyn virt::PartitionMemoryMap> {
        assert_eq!(vtl, hvdef::Vtl::Vtl0);
        self.inner.clone()
    }
}

// TODO: figure out a better abstraction that works for both KVM and WHP.
impl virt::PartitionMemoryMap for KvmPartitionInner {
    unsafe fn map_range(
        &self,
        data: *mut u8,
        size: usize,
        addr: u64,
        writable: bool,
        _exec: bool,
    ) -> anyhow::Result<()> {
        // SAFETY: `PartitionMemoryMap::map_range` requires the caller to keep
        // `data..data+size` valid for the lifetime of the mapping. `map_region`
        // preserves that lifetime requirement and records the mapped range so
        // it can be cleared on unmap.
        unsafe { self.map_region(data, size, addr, !writable) }
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size);
        let mut state = self.memory.lock();
        for (slot, entry) in state.ranges.iter_mut().enumerate() {
            let Some(kvm_range) = entry else { continue };
            if range.contains(&kvm_range.range) {
                let guest_memfd_backed = kvm_range.guest_memfd_offset.is_some();
                if kvm_range
                    .private_state
                    .is_some_and(KvmGuestMemfdPrivateState::uses_vm_attributes)
                {
                    self.kvm.set_memory_attributes(
                        kvm_range.range.start(),
                        kvm_range.range.len(),
                        0,
                    )?;
                }
                #[cfg(guest_arch = "aarch64")]
                if kvm_range.private_state == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault) {
                    let guest_memfd_offset = kvm_range
                        .guest_memfd_offset
                        .ok_or(MemoryError::InvalidPrivateMemoryRange)?;
                    if let Err(err) = self.discard_stale_private_memory_backing(
                        &[KvmMemoryRangeSegment {
                            range: kvm_range.range,
                            host_addr: kvm_range.host_addr,
                            guest_memfd_offset,
                        }],
                        false,
                        "CCA slot unmap",
                    ) {
                        self.mark_cca_fatal();
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed CCA slot backing cleanup; partition marked fatal"
                        );
                    }
                }
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                // TODO: This error propagates to `PartitionMapper::unmap_region`,
                // which currently panics because partition unmap is treated as
                // infallible. Any recoverable policy must keep the slot's backing
                // VA valid until the slot is cleared.
                if let Err(err) = unsafe { self.clear_slot(slot, guest_memfd_backed) } {
                    #[cfg(guest_arch = "aarch64")]
                    if kvm_range.private_state == Some(KvmGuestMemfdPrivateState::GuestMemfdDefault)
                    {
                        self.mark_cca_fatal();
                        tracing::error!(
                            error = &err as &dyn std::error::Error,
                            "failed to clear CCA slot; partition marked fatal"
                        );
                        return Err(err.into());
                    }
                    return Err(err.into());
                }
                *entry = None;
            } else {
                assert!(
                    !range.overlaps(&kvm_range.range),
                    "can only unmap existing ranges of exact size"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> MemoryRange {
        MemoryRange::new(start..end)
    }

    fn guest_memfd_ranges(ranges: &[MemoryRange]) -> Vec<KvmGuestMemfdRange> {
        let mut file_offset = 0;
        ranges
            .iter()
            .map(|&range| {
                let guest_memfd_range = KvmGuestMemfdRange { range, file_offset };
                file_offset += range.len();
                guest_memfd_range
            })
            .collect()
    }

    fn test_private_state() -> KvmGuestMemfdPrivateState {
        #[cfg(guest_arch = "x86_64")]
        {
            KvmGuestMemfdPrivateState::VmAttributes
        }
        #[cfg(guest_arch = "aarch64")]
        {
            KvmGuestMemfdPrivateState::GuestMemfdDefault
        }
    }

    #[test]
    fn guest_memfd_classifier_selects_contained_ram() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges).unwrap(),
            Some(0x1000)
        );
        assert_eq!(
            classify_guest_memfd_backing(range(0x1_1000, 0x1_3000), &ram_ranges).unwrap(),
            Some(0x9000)
        );
    }

    #[test]
    fn guest_memfd_classifier_keeps_non_ram_userspace() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0xa000, 0xc000), &ram_ranges).unwrap(),
            None
        );
    }

    #[test]
    fn guest_memfd_classifier_rejects_partial_ram_overlap() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x8000, 0xa000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_does_not_merge_adjacent_ram_ranges() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x3000), range(0x3000, 0x5000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_rejects_ambiguous_ram_containment() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x5000), range(0x2000, 0x4000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(MemoryError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn private_memory_range_resolves_hva_offset() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd_offset: Some(0),
            private_state: Some(test_private_state()),
        })];

        let resolved = private_memory_range_from_slots(range(0x3000, 0x5000), &slots).unwrap();

        assert_eq!(resolved.gpa, range(0x3000, 0x5000));
        assert_eq!(resolved.hva, host_addr.wrapping_add(0x2000));
    }

    #[test]
    fn private_memory_range_rejects_non_private_or_non_guest_memfd_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let userspace_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd_offset: None,
            private_state: Some(test_private_state()),
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &userspace_slots),
            Err(MemoryError::InvalidPrivateMemoryRange)
        ));

        let shared_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd_offset: Some(0),
            private_state: None,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &shared_slots),
            Err(MemoryError::InvalidPrivateMemoryRange)
        ));
    }

    #[test]
    fn guest_memfd_segments_cover_adjacent_unordered_slots() {
        let mut first_backing = vec![0u8; 0x2000];
        let mut second_backing = vec![0u8; 0x2000];
        let first_host_addr = first_backing.as_mut_ptr();
        let second_host_addr = second_backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr: second_host_addr,
                range: range(0x3000, 0x5000),
                guest_memfd_offset: Some(0x8000),
                private_state: None,
            }),
            Some(KvmMemoryRange {
                host_addr: first_host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd_offset: Some(0x4000),
                private_state: Some(test_private_state()),
            }),
        ];

        let segments = guest_memfd_range_segments(range(0x2000, 0x4000), &slots).unwrap();

        assert_eq!(
            segments,
            [
                KvmMemoryRangeSegment {
                    range: range(0x2000, 0x3000),
                    host_addr: first_host_addr.wrapping_add(0x1000),
                    guest_memfd_offset: 0x5000,
                },
                KvmMemoryRangeSegment {
                    range: range(0x3000, 0x4000),
                    host_addr: second_host_addr,
                    guest_memfd_offset: 0x8000,
                },
            ]
        );
    }

    #[test]
    fn guest_memfd_segments_reject_incomplete_coverage() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let gapped_slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x2000),
                guest_memfd_offset: Some(0),
                private_state: Some(test_private_state()),
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x2000),
                range: range(0x3000, 0x4000),
                guest_memfd_offset: Some(0x2000),
                private_state: Some(test_private_state()),
            }),
        ];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &gapped_slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));

        let userspace_slot = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x4000),
            guest_memfd_offset: None,
            private_state: None,
        })];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &userspace_slot),
            Err(MemoryError::InvalidMapGpaRange)
        ));
    }

    #[test]
    fn guest_memfd_intersections_allow_unbacked_ranges() {
        let mut backing = vec![0u8; 0x1000];
        let host_addr = backing.as_mut_ptr();
        let slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x2000, 0x3000),
            guest_memfd_offset: Some(0x4000),
            private_state: Some(test_private_state()),
        })];

        let segments = guest_memfd_range_intersections(range(0x1000, 0x4000), &slots).unwrap();
        assert_eq!(
            segments,
            [KvmMemoryRangeSegment {
                range: range(0x2000, 0x3000),
                host_addr,
                guest_memfd_offset: 0x4000,
            }]
        );
        assert!(
            guest_memfd_range_intersections(range(0x4000, 0x5000), &slots)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn guest_memfd_segments_reject_overlapping_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd_offset: Some(0),
                private_state: Some(test_private_state()),
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x1000),
                range: range(0x2000, 0x4000),
                guest_memfd_offset: Some(0x1000),
                private_state: Some(test_private_state()),
            }),
        ];

        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));
        assert!(matches!(
            guest_memfd_range_intersections(range(0x1000, 0x4000), &slots),
            Err(MemoryError::InvalidMapGpaRange)
        ));
    }
}
