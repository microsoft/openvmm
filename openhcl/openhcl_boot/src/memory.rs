// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Address space allocator for VTL2 memory used by the bootshim.

use crate::host_params::MAX_VTL2_RAM_RANGES;
use arrayvec::ArrayVec;
use core::panic;
use host_fdt_parser::MemoryEntry;
#[cfg(test)]
use igvm_defs::MemoryMapEntryType;
use igvm_defs::PAGE_SIZE_4K;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use memory_range::RangeWalkResult;
use memory_range::walk_ranges;

/// The maximum number of reserved memory ranges that we might use.
/// See [`ReservedMemoryType`] definition for details.
pub const MAX_RESERVED_MEM_RANGES: usize = 6 + sidecar_defs::MAX_NODES;

const MAX_MEMORY_RANGES: usize = MAX_VTL2_RAM_RANGES + MAX_RESERVED_MEM_RANGES;

/// Maximum number of ranges in the address space manager.
/// TODO: sizing of arrayvec
const MAX_ADDRESS_RANGES: usize = MAX_MEMORY_RANGES * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservedMemoryType {
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
    /// Page tables that are used for AP startup, on TDX.
    TdxPageTables,
}

impl From<ReservedMemoryType> for MemoryVtlType {
    fn from(r: ReservedMemoryType) -> Self {
        match r {
            ReservedMemoryType::Vtl2Config => MemoryVtlType::VTL2_CONFIG,
            ReservedMemoryType::SidecarImage => MemoryVtlType::VTL2_SIDECAR_IMAGE,
            ReservedMemoryType::SidecarNode => MemoryVtlType::VTL2_SIDECAR_NODE,
            ReservedMemoryType::Vtl2Reserved => MemoryVtlType::VTL2_RESERVED,
            ReservedMemoryType::Vtl2GpaPool => MemoryVtlType::VTL2_GPA_POOL,
            ReservedMemoryType::TdxPageTables => MemoryVtlType::VTL2_TDX_PAGE_TABLES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddressUsage {
    /// free for allocation
    Free,
    /// used by the bootshim (usually build time), but free for kernel use
    Used,
    /// reserved for some reason
    Reserved(ReservedMemoryType),
}

#[derive(Debug)]
struct AddressRange {
    range: MemoryRange,
    vnode: u32,
    usage: AddressUsage,
}

impl From<AddressUsage> for MemoryVtlType {
    fn from(usage: AddressUsage) -> Self {
        match usage {
            AddressUsage::Free => MemoryVtlType::VTL2_RAM,
            AddressUsage::Used => MemoryVtlType::VTL2_RAM,
            AddressUsage::Reserved(r) => r.into(),
        }
    }
}

#[derive(Debug)]
pub struct AllocatedRange {
    pub range: MemoryRange,
    pub vnode: u32,
}

#[derive(Debug)]
pub struct AddressSpaceManager {
    /// tracks address space, must be sorted
    address_space: ArrayVec<AddressRange, MAX_ADDRESS_RANGES>,
}

impl AddressSpaceManager {
    pub const fn new_const() -> Self {
        Self {
            address_space: ArrayVec::new_const(),
        }
    }

    /// Initialize the address space manager.
    ///
    /// `bootshim_used`
    /// Some regions are known to be used at construction time, as these ranges
    /// are allocated at build time.
    pub fn init(
        &mut self,
        vtl2_ram: &[MemoryEntry],
        bootshim_used: MemoryRange,
        vtl2_config: impl Iterator<Item = MemoryRange>,
        reserved_range: Option<MemoryRange>,
        sidecar_image: Option<MemoryRange>,
        page_tables: Option<MemoryRange>,
    ) {
        // assert!(vtl2_config.len() <= 2);
        assert!(vtl2_ram.len() <= MAX_VTL2_RAM_RANGES);
        assert!(self.address_space.is_empty());

        // The other ranges are reserved, and may overlap with the used range.
        let mut reserved: ArrayVec<(MemoryRange, ReservedMemoryType), 5> = ArrayVec::new();
        reserved.extend(vtl2_config.map(|r| (r, ReservedMemoryType::Vtl2Config)));
        reserved.extend(
            reserved_range
                .into_iter()
                .map(|r| (r, ReservedMemoryType::Vtl2Reserved)),
        );
        reserved.extend(
            sidecar_image
                .into_iter()
                .map(|r| (r, ReservedMemoryType::SidecarImage)),
        );
        reserved.extend(
            page_tables
                .into_iter()
                .map(|r| (r, ReservedMemoryType::TdxPageTables)),
        );
        reserved.sort_unstable_by_key(|(r, _)| r.start());

        let mut used_ranges: ArrayVec<(MemoryRange, AddressUsage), 10> = ArrayVec::new();

        // Construct initial used ranges by walking both the bootshim_used range
        // and all reserved ranges that overlap.
        for (entry, r) in walk_ranges(
            core::iter::once((bootshim_used, AddressUsage::Used)),
            reserved.iter().cloned(),
        ) {
            match r {
                RangeWalkResult::Left(_) => {
                    used_ranges.push((entry, AddressUsage::Used));
                }
                RangeWalkResult::Both(_, reserved_type) => {
                    used_ranges.push((entry, AddressUsage::Reserved(reserved_type)));
                }
                RangeWalkResult::Right(usage) => {
                    panic!(
                        "reserved range {r:#x?} used by {usage:?} not contained in bootshim_used {bootshim_used:#x?}"
                    );
                }
                RangeWalkResult::Neither => {}
            }
        }

        // Construct the initial state of VTL2 address space by walking ram and reserved ranges
        assert!(self.address_space.is_empty());
        for (entry, r) in walk_ranges(
            vtl2_ram.iter().map(|e| (e.range, e.vnode)),
            used_ranges.iter().map(|(r, usage)| (*r, usage)),
        ) {
            match r {
                RangeWalkResult::Left(vnode) => {
                    // VTL2 normal ram, unused by anything.
                    self.address_space.push(AddressRange {
                        range: entry,
                        vnode,
                        usage: AddressUsage::Free,
                    });
                }
                RangeWalkResult::Both(vnode, usage) => {
                    // VTL2 ram, currently in use.
                    self.address_space.push(AddressRange {
                        range: entry,
                        vnode,
                        usage: *usage,
                    });
                }
                RangeWalkResult::Right(usage) => {
                    panic!("vtl2 range {entry:#x?} used by {usage:?} not contained in vtl2 ram");
                }
                RangeWalkResult::Neither => {}
            }
        }
    }

    /// Split a free range into two, with allocation policy deciding if we
    /// allocate the low part or high part.
    fn allocate_range(
        &mut self,
        index: usize,
        len: u64,
        usage: AddressUsage,
        allocation_policy: AllocationPolicy,
    ) -> AllocatedRange {
        assert!(usage != AddressUsage::Free);
        let range = self.address_space.get_mut(index).expect("valid index");
        assert_eq!(range.usage, AddressUsage::Free);
        assert!(range.range.len() >= len);

        let (used, remainder) = match allocation_policy {
            AllocationPolicy::LowMemory => {
                // Allocate from the beginning (low addresses)
                range.range.split_at_offset(len)
            }
            AllocationPolicy::HighMemory => {
                // Allocate from the end (high addresses)
                let offset = range.range.len() - len;
                let (remainder, used) = range.range.split_at_offset(offset);
                (used, remainder)
            }
        };

        let remainder = if !remainder.is_empty() {
            Some(AddressRange {
                range: remainder,
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
            match allocation_policy {
                AllocationPolicy::LowMemory => {
                    // When allocating from low memory, the remainder goes after
                    // the allocated range
                    self.address_space.insert(index + 1, remainder);
                }
                AllocationPolicy::HighMemory => {
                    // When allocating from high memory, the remainder goes
                    // before the allocated range
                    self.address_space.insert(index, remainder);
                }
            }
        }

        allocated
    }

    pub fn allocate(
        &mut self,
        preferred_vnode: Option<u32>,
        len: u64,
        allocation_type: AllocationType,
        allocation_policy: AllocationPolicy,
    ) -> Option<AllocatedRange> {
        // len must be page aligned
        assert_eq!(len % PAGE_SIZE_4K, 0);

        fn find_index<'a>(
            mut iter: impl Iterator<Item = (usize, &'a AddressRange)>,
            preferred_vnode: Option<u32>,
            len: u64,
        ) -> Option<usize> {
            iter.find_map(|(index, range)| {
                if range.usage == AddressUsage::Free
                    && range.range.len() >= len
                    && preferred_vnode.map(|pv| pv == range.vnode).unwrap_or(true)
                {
                    Some(index)
                } else {
                    None
                }
            })
        }

        // Walk ranges in reverse order until one is free that has enough space
        let index = {
            let iter = self.address_space.iter().enumerate();
            match allocation_policy {
                AllocationPolicy::LowMemory => find_index(iter, preferred_vnode, len),
                AllocationPolicy::HighMemory => find_index(iter.rev(), preferred_vnode, len),
            }
        };

        index.map(|index| {
            self.allocate_range(
                index,
                len,
                match allocation_type {
                    AllocationType::GpaPool => {
                        AddressUsage::Reserved(ReservedMemoryType::Vtl2GpaPool)
                    }
                    AllocationType::SidecarNode => {
                        AddressUsage::Reserved(ReservedMemoryType::SidecarNode)
                    }
                },
                allocation_policy,
            )
        })
    }

    /// Get all of vtl2 address space
    pub fn vtl2_ranges(&self) -> impl Iterator<Item = (MemoryRange, MemoryVtlType)> + use<'_> {
        // FIXME FLATTEN RANGES via flatten_ranges
        self.address_space.iter().map(|r| (r.range, r.usage.into()))
    }

    /// Get only reserved vtl2 ranges that are not described as e820 ranges
    pub fn reserved_vtl2_ranges(
        &self,
    ) -> impl Iterator<Item = (MemoryRange, ReservedMemoryType)> + use<'_> {
        self.address_space.iter().filter_map(|r| match r.usage {
            AddressUsage::Reserved(typ) => Some((r.range, typ)),
            _ => None,
        })
    }
}

pub enum AllocationType {
    GpaPool,
    SidecarNode,
}

pub enum AllocationPolicy {
    // prefer low memory
    LowMemory,
    // prefer high memory
    // TODO: only used in tests, but will be used in an upcoming change
    #[allow(dead_code)]
    HighMemory,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocate() {
        let mut address_space = AddressSpaceManager::new_const();
        address_space.init(
            &[MemoryEntry {
                range: MemoryRange::new(0x0..0x20000),
                vnode: 0,
                mem_type: MemoryMapEntryType::MEMORY,
            }],
            MemoryRange::new(0x0..0xF000),
            [
                MemoryRange::new(0x3000..0x4000),
                MemoryRange::new(0x5000..0x6000),
            ]
            .iter()
            .cloned(),
            Some(MemoryRange::new(0x8000..0xA000)),
            Some(MemoryRange::new(0xA000..0xC000)),
            None,
        );

        let range = address_space
            .allocate(
                None,
                0x1000,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1F000..0x20000));

        let range = address_space
            .allocate(
                None,
                0x2000,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1D000..0x1F000));

        let range = address_space
            .allocate(
                None,
                0x3000,
                AllocationType::GpaPool,
                AllocationPolicy::LowMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0xF000..0x12000));

        let range = address_space
            .allocate(
                None,
                0x1000,
                AllocationType::GpaPool,
                AllocationPolicy::LowMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x12000..0x13000));
    }

    // test numa allocation
    #[test]
    fn test_allocate_numa() {
        let mut address_space = AddressSpaceManager::new_const();
        address_space.init(
            &[
                MemoryEntry {
                    range: MemoryRange::new(0x0..0x20000),
                    vnode: 0,
                    mem_type: MemoryMapEntryType::MEMORY,
                },
                MemoryEntry {
                    range: MemoryRange::new(0x20000..0x40000),
                    vnode: 1,
                    mem_type: MemoryMapEntryType::MEMORY,
                },
                MemoryEntry {
                    range: MemoryRange::new(0x40000..0x60000),
                    vnode: 2,
                    mem_type: MemoryMapEntryType::MEMORY,
                },
                MemoryEntry {
                    range: MemoryRange::new(0x60000..0x80000),
                    vnode: 3,
                    mem_type: MemoryMapEntryType::MEMORY,
                },
            ],
            MemoryRange::new(0x0..0x10000),
            [
                MemoryRange::new(0x3000..0x4000),
                MemoryRange::new(0x5000..0x6000),
            ]
            .iter()
            .cloned(),
            Some(MemoryRange::new(0x8000..0xA000)),
            Some(MemoryRange::new(0xA000..0xC000)),
            None,
        );

        let range = address_space
            .allocate(
                Some(0),
                0x1000,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1F000..0x20000));
        assert_eq!(range.vnode, 0);

        let range = address_space
            .allocate(
                Some(0),
                0x2000,
                AllocationType::SidecarNode,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1D000..0x1F000));
        assert_eq!(range.vnode, 0);

        let range = address_space
            .allocate(
                Some(2),
                0x3000,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x5D000..0x60000));
        assert_eq!(range.vnode, 2);

        // allocate all of node 3, then subsequent allocations fail
        let range = address_space
            .allocate(
                Some(3),
                0x20000,
                AllocationType::SidecarNode,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x60000..0x80000));
        assert_eq!(range.vnode, 3);

        let range = address_space.allocate(
            Some(3),
            0x1000,
            AllocationType::SidecarNode,
            AllocationPolicy::HighMemory,
        );
        assert!(
            range.is_none(),
            "allocation should fail, no space left for node 3"
        );
    }
}
