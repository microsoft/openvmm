// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::KvmError;
use crate::KvmPartition;
use crate::KvmPartitionInner;
use inspect::Inspect;
use memory_range::MemoryRange;
use std::fs::File;
use std::sync::Arc;

#[derive(Debug, Inspect)]
pub(crate) struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
    guest_memfd_offset: Option<u64>,
    private_attributes_set: bool,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
pub(crate) struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    pub(crate) ranges: Vec<Option<KvmMemoryRange>>,
}

#[derive(Debug, Inspect)]
#[inspect(external_tag)]
pub(crate) enum KvmMemoryBackingMode {
    Userspace,
    GuestMemfd(KvmGuestMemfdBacking),
}

#[derive(Debug, Inspect)]
pub(crate) struct KvmGuestMemfdBacking {
    #[inspect(skip)]
    file: File,
    #[inspect(iter_by_index)]
    ranges: Vec<KvmGuestMemfdRange>,
    initial_private: bool,
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
        initial_private: bool,
    },
}

impl KvmMemoryBackingMode {
    #[expect(dead_code)]
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
            KvmMemoryBacking::GuestMemfd {
                file,
                file_offset,
                initial_private,
            } => {
                // SAFETY: `map_region` requires its caller to keep
                // `data..data+size` valid until this guest-physical range is
                // unmapped or the partition is destroyed. `memory_backing`
                // The partition owns the backing guestmemfd for at least as long
                // as KVM references it.
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
}

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
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

    #[test]
    fn guest_memfd_classifier_does_not_merge_adjacent_ram_ranges() {
        let ram_ranges = guest_memfd_ranges(&[range(0x1000, 0x3000), range(0x3000, 0x5000)]);

        assert!(matches!(
            classify_guest_memfd_backing(range(0x2000, 0x4000), &ram_ranges),
            Err(KvmError::UnsupportedIsolationConfiguration(_))
        ));
    }

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
}
