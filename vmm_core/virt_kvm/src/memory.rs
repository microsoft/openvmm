// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! KVM memory-slot and confidential guest backing management.
//!
//! Confidential RAM slots use userspace memory for shared access and a
//! guestmemfd for private access. This module records both sides of each slot,
//! selects the appropriate backing when a range is mapped, validates private
//! launch ranges, and discards stale contents when ownership changes.

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use inspect::Inspect;
use memory_range::MemoryRange;
#[cfg(guest_arch = "x86_64")]
use std::fs::File;
#[cfg(guest_arch = "x86_64")]
use std::os::fd::AsRawFd;
use std::sync::Arc;

#[derive(Debug, Inspect)]
/// A registered KVM memory slot and its confidential-memory metadata.
pub(crate) struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
    guest_memfd_offset: Option<u64>,
    private_attributes_set: bool,
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

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct KvmMemoryRangeSegment {
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
    #[cfg(guest_arch = "x86_64")]
    /// Register shared userspace and private guestmemfd backing for RAM.
    GuestMemfd(KvmGuestMemfdBacking),
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Inspect)]
/// Partition-owned guestmemfd and its packed mapping of guest RAM ranges.
pub(crate) struct KvmGuestMemfdBacking {
    #[inspect(skip)]
    file: File,
    #[inspect(iter_by_index)]
    ranges: Vec<KvmGuestMemfdRange>,
    initial_private: bool,
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug, Copy, Clone, Eq, PartialEq, Inspect)]
struct KvmGuestMemfdRange {
    range: MemoryRange,
    file_offset: u64,
}

#[cfg(guest_arch = "x86_64")]
#[derive(Debug)]
enum KvmMemoryBacking<'a> {
    Userspace,
    GuestMemfd {
        file: &'a File,
        file_offset: u64,
        initial_private: bool,
    },
}

#[cfg(guest_arch = "aarch64")]
#[derive(Debug)]
enum KvmMemoryBacking {
    Userspace,
}

impl KvmMemoryBackingMode {
    #[cfg(guest_arch = "x86_64")]
    /// Creates one guestmemfd spanning the supplied RAM ranges.
    ///
    /// Guest ranges are packed contiguously into the file in iteration order.
    /// `initial_private` controls both the initial memory attributes and which
    /// backing KVM selects when each slot is first registered.
    pub(crate) fn guest_memfd(
        kvm: &kvm::Partition,
        ram_ranges: impl IntoIterator<Item = MemoryRange>,
        initial_private: bool,
    ) -> Result<Self, KvmError> {
        check_private_memory_extensions(kvm)?;

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
            initial_private,
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
                return Err(KvmError::CannotResizeGuestMemfdSlot.into());
            }
            if existing_range.private_attributes_set {
                self.kvm.set_memory_attributes(
                    existing_range.range.start(),
                    existing_range.range.len(),
                    0,
                )?;
            }
            if existing_range.guest_memfd_offset.is_some() {
                // SAFETY: clearing a slot removes the memory reference.
                unsafe { self.clear_slot(slot_to_use, true)? };
                state.ranges[slot_to_use] = None;
            }
        }
        let (guest_memfd_offset, private_attributes_set) = match backing {
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
                (None, false)
            }
            #[cfg(guest_arch = "x86_64")]
            KvmMemoryBacking::GuestMemfd {
                file,
                file_offset,
                initial_private,
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
                if initial_private {
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
                (Some(file_offset), initial_private)
            }
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range,
            guest_memfd_offset,
            private_attributes_set,
        });
        Ok(())
    }

    #[cfg(guest_arch = "x86_64")]
    fn memory_backing(&self, range: MemoryRange) -> Result<KvmMemoryBacking<'_>, KvmError> {
        match &self.memory_backing_mode {
            KvmMemoryBackingMode::Userspace => Ok(KvmMemoryBacking::Userspace),
            KvmMemoryBackingMode::GuestMemfd(backing) => {
                match classify_guest_memfd_backing(range, &backing.ranges)? {
                    Some(file_offset) => Ok(KvmMemoryBacking::GuestMemfd {
                        file: &backing.file,
                        file_offset,
                        initial_private: backing.initial_private,
                    }),
                    None => Ok(KvmMemoryBacking::Userspace),
                }
            }
        }
    }

    #[cfg(guest_arch = "aarch64")]
    fn memory_backing(&self, _range: MemoryRange) -> Result<KvmMemoryBacking, KvmError> {
        Ok(KvmMemoryBacking::Userspace)
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
    ) -> Result<(), KvmError> {
        const KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK: u64 = 0x3;
        const KVM_MAP_GPA_RANGE_ENC_STATUS_MASK: u64 = 0xf << 4;

        let size = page_count
            .checked_mul(hvdef::HV_PAGE_SIZE)
            .ok_or(KvmError::InvalidMapGpaRange)?;
        let end = gpa.checked_add(size).ok_or(KvmError::InvalidMapGpaRange)?;
        if !gpa.is_multiple_of(hvdef::HV_PAGE_SIZE) || size == 0 {
            return Err(KvmError::InvalidMapGpaRange);
        }
        let unsupported_attributes = map_attributes
            & !(KVM_MAP_GPA_RANGE_PAGE_SIZE_MASK | KVM_MAP_GPA_RANGE_ENC_STATUS_MASK);
        if unsupported_attributes != 0 {
            return Err(KvmError::UnsupportedMapGpaRangeAttributes(map_attributes));
        }
        let private = match map_attributes & KVM_MAP_GPA_RANGE_ENC_STATUS_MASK {
            kvm::KVM_MAP_GPA_RANGE_DECRYPTED_UAPI => false,
            kvm::KVM_MAP_GPA_RANGE_ENCRYPTED_UAPI => true,
            _ => return Err(KvmError::UnsupportedMapGpaRangeAttributes(map_attributes)),
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

    #[cfg(guest_arch = "x86_64")]
    /// Discards data from the backing that is no longer selected by KVM.
    ///
    /// Guestmemfd memory slots have separate shared userspace and private
    /// guestmemfd backings. For a shared-to-private conversion, discard the
    /// shared mapping with `MADV_DONTNEED`. For a private-to-shared conversion,
    /// punch a hole in guestmemfd so private data cannot become visible after a
    /// later conversion back to private.
    fn discard_stale_private_memory_backing(
        &self,
        segments: &[KvmMemoryRangeSegment],
        private: bool,
        isolation_name: &'static str,
    ) -> Result<(), KvmError> {
        if private {
            for segment in segments {
                tracing::debug!(
                    gpa = segment.range.start(),
                    size = segment.range.len(),
                    hva = segment.host_addr as usize,
                    isolation_name,
                    "discarding shared backing after private conversion"
                );
                let ret = unsafe {
                    libc::madvise(
                        segment.host_addr.cast(),
                        segment.range.len() as usize,
                        libc::MADV_DONTNEED,
                    )
                };
                if ret != 0 {
                    return Err(KvmError::DiscardSharedBacking(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
        } else {
            let KvmMemoryBackingMode::GuestMemfd(backing) = &self.memory_backing_mode else {
                return Err(KvmError::InvalidMapGpaRange);
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
                    return Err(KvmError::DiscardPrivateBacking(
                        std::io::Error::last_os_error(),
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg(guest_arch = "x86_64")]
fn guest_memfd_range_segments(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<Vec<KvmMemoryRangeSegment>, KvmError> {
    let mut segments = slots
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
        .collect::<Vec<_>>();
    segments.sort_by_key(|segment| segment.range.start());

    let mut cursor = range.start();
    for segment in &segments {
        if segment.range.start() != cursor {
            return Err(KvmError::InvalidMapGpaRange);
        }
        cursor = segment.range.end();
    }
    if cursor != range.end() {
        return Err(KvmError::InvalidMapGpaRange);
    }

    Ok(segments)
}

#[cfg_attr(guest_arch = "aarch64", expect(dead_code))]
/// Resolves an imported range to a private guestmemfd slot and source HVA.
///
/// The entire range must be contained in one slot whose private attribute is
/// already active.
pub(crate) fn private_memory_range_from_slots(
    range: MemoryRange,
    slots: &[Option<KvmMemoryRange>],
) -> Result<KvmPrivateMemoryRange, KvmError> {
    let slot = slots
        .iter()
        .flatten()
        .find(|slot| slot.range.contains(&range))
        .ok_or(KvmError::InvalidPrivateMemoryRange)?;

    if slot.guest_memfd_offset.is_none() || !slot.private_attributes_set {
        return Err(KvmError::InvalidPrivateMemoryRange);
    }

    let offset = range.start() - slot.range.start();
    Ok(KvmPrivateMemoryRange {
        gpa: range,
        hva: slot.host_addr.wrapping_add(offset as usize),
    })
}

#[cfg(guest_arch = "x86_64")]
/// Verifies the KVM capabilities required for guestmemfd private memory.
fn check_private_memory_extensions(kvm: &kvm::Partition) -> Result<(), KvmError> {
    require_kvm_extension(kvm, kvm::KVM_CAP_USER_MEMORY2, "KVM_CAP_USER_MEMORY2")?;
    require_kvm_extension(kvm, kvm::KVM_CAP_GUEST_MEMFD, "KVM_CAP_GUEST_MEMFD")?;
    let memory_attributes = require_kvm_extension(
        kvm,
        kvm::KVM_CAP_MEMORY_ATTRIBUTES,
        "KVM_CAP_MEMORY_ATTRIBUTES",
    )?;
    if memory_attributes as u64 & kvm::KVM_MEMORY_ATTRIBUTE_PRIVATE as u64 == 0 {
        return Err(kvm::Error::MissingCapability(
            "KVM_CAP_MEMORY_ATTRIBUTES(KVM_MEMORY_ATTRIBUTE_PRIVATE)",
        )
        .into());
    }
    Ok(())
}

#[cfg(guest_arch = "x86_64")]
fn require_kvm_extension(
    kvm: &kvm::Partition,
    extension: u32,
    capability: &'static str,
) -> Result<i32, KvmError> {
    let value = kvm
        .check_extension(extension)
        .map_err(kvm::Error::CheckExtension)?;
    if value == 0 {
        return Err(kvm::Error::MissingCapability(capability).into());
    }
    Ok(value)
}

#[cfg(guest_arch = "x86_64")]
fn classify_guest_memfd_backing(
    range: MemoryRange,
    ram_ranges: &[KvmGuestMemfdRange],
) -> Result<Option<u64>, KvmError> {
    let mut containing_ranges = ram_ranges
        .iter()
        .filter(|ram_range| ram_range.range.contains(&range));
    if let Some(ram_range) = containing_ranges.next() {
        if containing_ranges.next().is_some() {
            return Err(KvmError::UnsupportedIsolationConfiguration(
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
        return Err(KvmError::UnsupportedIsolationConfiguration(
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
                if kvm_range.private_attributes_set {
                    self.kvm.set_memory_attributes(
                        kvm_range.range.start(),
                        kvm_range.range.len(),
                        0,
                    )?;
                }
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                unsafe { self.clear_slot(slot, guest_memfd_backed)? };
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

    #[cfg(guest_arch = "x86_64")]
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

    #[derive(Debug, Copy, Clone, Eq, PartialEq)]
    struct KvmPrivateMemoryRange {
        gpa: MemoryRange,
        hva: *mut u8,
    }

    fn private_memory_range_from_slots(
        range: MemoryRange,
        slots: &[Option<KvmMemoryRange>],
    ) -> Result<KvmPrivateMemoryRange, KvmError> {
        let slot = slots
            .iter()
            .flatten()
            .find(|slot| slot.range.contains(&range))
            .ok_or(KvmError::InvalidPrivateMemoryRange)?;

        if slot.guest_memfd_offset.is_none() || !slot.private_attributes_set {
            return Err(KvmError::InvalidPrivateMemoryRange);
        }

        let offset = range.start() - slot.range.start();
        Ok(KvmPrivateMemoryRange {
            gpa: range,
            hva: slot.host_addr.wrapping_add(offset as usize),
        })
    }

    #[cfg(guest_arch = "x86_64")]
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

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_keeps_non_ram_userspace() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert_eq!(
            classify_guest_memfd_backing(range(0xa000, 0xc000), &ram_ranges).unwrap(),
            None
        );
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_rejects_partial_ram_overlap() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x9000), range(0x1_0000, 0x2_0000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x8000, 0xa000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_does_not_merge_adjacent_ram_ranges() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x3000), range(0x3000, 0x5000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[cfg(guest_arch = "x86_64")]
    #[test]
    fn guest_memfd_classifier_rejects_ambiguous_ram_containment() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x5000), range(0x2000, 0x4000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
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
            private_attributes_set: true,
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
            private_attributes_set: true,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &userspace_slots),
            Err(KvmError::InvalidPrivateMemoryRange)
        ));

        let shared_slots = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x5000),
            guest_memfd_offset: Some(0),
            private_attributes_set: false,
        })];
        assert!(matches!(
            private_memory_range_from_slots(range(0x1000, 0x2000), &shared_slots),
            Err(KvmError::InvalidPrivateMemoryRange)
        ));
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
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
                private_attributes_set: false,
            }),
            Some(KvmMemoryRange {
                host_addr: first_host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd_offset: Some(0x4000),
                private_attributes_set: true,
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
    #[cfg(guest_arch = "x86_64")]
    fn guest_memfd_segments_reject_incomplete_coverage() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let gapped_slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x2000),
                guest_memfd_offset: Some(0),
                private_attributes_set: true,
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x2000),
                range: range(0x3000, 0x4000),
                guest_memfd_offset: Some(0x2000),
                private_attributes_set: true,
            }),
        ];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &gapped_slots),
            Err(KvmError::InvalidMapGpaRange)
        ));

        let userspace_slot = [Some(KvmMemoryRange {
            host_addr,
            range: range(0x1000, 0x4000),
            guest_memfd_offset: None,
            private_attributes_set: false,
        })];
        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &userspace_slot),
            Err(KvmError::InvalidMapGpaRange)
        ));
    }

    #[test]
    #[cfg(guest_arch = "x86_64")]
    fn guest_memfd_segments_reject_overlapping_slots() {
        let mut backing = vec![0u8; 0x4000];
        let host_addr = backing.as_mut_ptr();
        let slots = [
            Some(KvmMemoryRange {
                host_addr,
                range: range(0x1000, 0x3000),
                guest_memfd_offset: Some(0),
                private_attributes_set: true,
            }),
            Some(KvmMemoryRange {
                host_addr: host_addr.wrapping_add(0x1000),
                range: range(0x2000, 0x4000),
                guest_memfd_offset: Some(0x1000),
                private_attributes_set: true,
            }),
        ];

        assert!(matches!(
            guest_memfd_range_segments(range(0x1000, 0x4000), &slots),
            Err(KvmError::InvalidMapGpaRange)
        ));
    }
}
