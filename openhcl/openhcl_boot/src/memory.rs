// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Address space allocator for VTL2 memory used by the bootshim.

use crate::host_params::MAX_VTL2_RAM_RANGES;
use arrayvec::ArrayVec;
use core::alloc;
use core::cell::RefCell;
use host_fdt_parser::MemoryEntry;
use igvm_defs::MemoryMapEntryType;
use memory_range::MemoryRange;

/// The maximum number of reserved memory ranges that we might use.
/// See ReservedMemoryType definition for details.
const MAX_RESERVED_MEM_RANGES: usize = 5 + sidecar_defs::MAX_NODES;

const MAX_MEMORY_RANGES: usize = MAX_VTL2_RAM_RANGES + MAX_RESERVED_MEM_RANGES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReservedMemoryType {
    /// VTL2 parameter regions (could be up to 2).
    Vtl2Config,
    /// Reserved memory that should not be used by the kernel or usermode. There
    /// should only be one.
    Vtl2Reserved,
    /// Sidecar image. There should only be one.
    SidecarImage,
    /// A reserved range per sidecar node.
    SidecarNode,
    /// Persistent VTL2 memory used for page allocations in usermode. This
    /// memory is persisted, both location and contents, across servicing.
    /// Today, we only support a single range.
    Vtl2GpaPool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressUsage {
    Free,
    Used,
    Reserved(ReservedMemoryType),
}

struct AddressRange {
    range: MemoryRange,
    mem_type: MemoryMapEntryType,
    vnode: u32,
    usage: AddressUsage,
}

pub struct AllocatedRange {
    pub range: MemoryRange,
    pub vnode: u32,
}

pub struct AddressSpaceManager {
    /// tracks address space, must be sorted
    address_space: ArrayVec<AddressRange, MAX_MEMORY_RANGES>,
}

impl AddressSpaceManager {
    /// Initialize the address space manager.
    ///
    /// Some regions are known to be used at construction time, as these ranges
    /// are allocated at boot time.
    pub fn new(
        vtl2_ram: &[MemoryEntry],
        vtl2_config: &[MemoryRange],
        reserved_range: MemoryRange,
        sidecar_image: Option<MemoryRange>,
    ) -> Self {
        assert!(vtl2_config.len() <= 2);
        assert!(vtl2_ram.len() <= MAX_VTL2_RAM_RANGES);

        todo!()
    }

    /// Split a free range into two, with the allocated range coming from the top end of the range.
    fn split_range(&mut self, index: usize, len: u64, usage: AddressUsage) -> AllocatedRange {
        assert!(usage != AddressUsage::Free);
        let range = self.address_space.get_mut(index).expect("valid index");
        assert_eq!(range.usage, AddressUsage::Free);

        let offset = range.range.len() - len;
        let (remainder, used) = range.range.split_at_offset(offset);
        let remainder = if !remainder.is_empty() {
            Some(AddressRange {
                range: remainder,
                mem_type: range.mem_type,
                vnode: range.vnode,
                usage: AddressUsage::Free,
            })
        } else {
            None
        };

        // Update this range to mark it as used
        range.usage = usage;
        range.range = used;
        let allocated = AllocatedRange {
            range: used,
            vnode: range.vnode,
        };

        if let Some(remainder) = remainder {
            self.address_space.insert(index, remainder);
        }

        allocated
    }
}
