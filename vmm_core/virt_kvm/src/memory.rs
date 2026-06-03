// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::KvmPartition;
use crate::KvmPartitionInner;
use inspect::Inspect;
use memory_range::MemoryRange;
use std::sync::Arc;

#[derive(Debug, Inspect)]
pub(crate) struct KvmMemoryRange {
    host_addr: *mut u8,
    range: MemoryRange,
}

unsafe impl Sync for KvmMemoryRange {}
unsafe impl Send for KvmMemoryRange {}

#[derive(Debug, Default, Inspect)]
pub(crate) struct KvmMemoryRangeState {
    #[inspect(flatten, iter_by_index)]
    pub(crate) ranges: Vec<Option<KvmMemoryRange>>,
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
        unsafe {
            self.kvm
                .set_user_memory_region(slot_to_use as u32, data, size, addr, readonly)?
        };
        state.ranges[slot_to_use] = Some(KvmMemoryRange {
            host_addr: data,
            range: MemoryRange::new(addr..addr + size as u64),
        });
        Ok(())
    }
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
        // SAFETY: guaranteed by caller.
        unsafe { self.map_region(data, size, addr, !writable) }
    }

    fn unmap_range(&self, addr: u64, size: u64) -> anyhow::Result<()> {
        let range = MemoryRange::new(addr..addr + size);
        let mut state = self.memory.lock();
        for (slot, entry) in state.ranges.iter_mut().enumerate() {
            let Some(kvm_range) = entry else { continue };
            if range.contains(&kvm_range.range) {
                // SAFETY: clearing a slot should always be safe since it removes
                // and does not add memory references.
                unsafe {
                    self.kvm.set_user_memory_region(
                        slot as u32,
                        std::ptr::null_mut(),
                        0,
                        0,
                        false,
                    )?;
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
