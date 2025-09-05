// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Address space allocator for VTL2 memory used by the bootshim.

use crate::host_params::MAX_VTL2_RAM_RANGES;
use arrayvec::ArrayVec;
use host_fdt_parser::MemoryEntry;
#[cfg(test)]
use igvm_defs::MemoryMapEntryType;
use loader_defs::shim::MemoryVtlType;
use memory_range::MemoryRange;
use memory_range::RangeWalkResult;
use memory_range::walk_ranges;
use thiserror::Error;

const PAGE_SIZE_4K: u64 = 4096;

/// The maximum number of reserved memory ranges that we might use.
/// See [`ReservedMemoryType`] definition for details.
pub const MAX_RESERVED_MEM_RANGES: usize = 6 + sidecar_defs::MAX_NODES;

const MAX_MEMORY_RANGES: usize = MAX_VTL2_RAM_RANGES + MAX_RESERVED_MEM_RANGES;

/// Maximum number of ranges in the address space manager.
/// For simplicity, make it twice the memory and reserved ranges.
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
    /// Free for allocation
    Free,
    /// Used by the bootshim (usually build time), but free for kernel use
    Used,
    /// Reserved and should not be reported to the kernel as usable RAM.
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

#[derive(Debug, Clone, Copy)]
pub struct AllocatedRange {
    pub range: MemoryRange,
    pub vnode: u32,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("ram len {len} greater than maximum {max}")]
    RamLen { len: u64, max: u64 },
    #[error("already initialized")]
    AlreadyInitialized,
    #[error(
        "reserved range {reserved:#x?}, type {typ:?} outside of bootshim used {bootshim_used:#x?}"
    )]
    ReservedRangeOutsideBootshimUsed {
        reserved: MemoryRange,
        typ: ReservedMemoryType,
        bootshim_used: MemoryRange,
    },
}

#[derive(Debug)]
pub struct AddressSpaceManager {
    /// Track the whole address space - this must be sorted.
    address_space: ArrayVec<AddressRange, MAX_ADDRESS_RANGES>,

    /// Track that the VTL2 GPA pool has at least one allocation.
    vtl2_pool: bool,
}

/// A builder used to initialize an [`AddressSpaceManager`].
pub struct AddressSpaceManagerBuilder<'a, I: Iterator<Item = MemoryRange>> {
    manager: &'a mut AddressSpaceManager,
    vtl2_ram: &'a [MemoryEntry],
    bootshim_used: MemoryRange,
    vtl2_config: I,
    reserved_range: Option<MemoryRange>,
    sidecar_image: Option<MemoryRange>,
    page_tables: Option<MemoryRange>,
}

impl<'a, I: Iterator<Item = MemoryRange>> AddressSpaceManagerBuilder<'a, I> {
    /// Create a new builder to initialize an [`AddressSpaceManager`].
    ///
    /// `vtl2_ram` is the list of ram ranges for VTL2, which must be sorted.
    ///
    /// `bootshim_used` is the range used by the bootshim, but may be reclaimed
    /// as ram by the kernel.
    ///
    /// Other ranges described by other methods must lie within `bootshim_used`.
    pub fn new(
        manager: &'a mut AddressSpaceManager,
        vtl2_ram: &'a [MemoryEntry],
        bootshim_used: MemoryRange,
        vtl2_config: I,
    ) -> AddressSpaceManagerBuilder<'a, I> {
        AddressSpaceManagerBuilder {
            manager,
            vtl2_ram,
            bootshim_used,
            vtl2_config,
            reserved_range: None,
            sidecar_image: None,
            page_tables: None,
        }
    }

    /// A reserved range reported as type [`MemoryVtlType::VTL2_RESERVED`].
    pub fn with_reserved_range(mut self, reserved_range: MemoryRange) -> Self {
        self.reserved_range = Some(reserved_range);
        self
    }

    /// The sidecar image, reported as type [`MemoryVtlType::VTL2_SIDECAR_IMAGE`].
    pub fn with_sidecar_image(mut self, sidecar_image: MemoryRange) -> Self {
        self.sidecar_image = Some(sidecar_image);
        self
    }

    /// Pagetables that are reported as type [`MemoryVtlType::VTL2_TDX_PAGE_TABLES`].
    pub fn with_page_tables(mut self, page_tables: MemoryRange) -> Self {
        self.page_tables = Some(page_tables);
        self
    }

    /// Consume the builder and initialize the address space manager.
    pub fn init(self) -> Result<&'a mut AddressSpaceManager, Error> {
        let Self {
            manager,
            vtl2_ram,
            bootshim_used,
            vtl2_config,
            reserved_range,
            sidecar_image,
            page_tables,
        } = self;

        if vtl2_ram.len() > MAX_VTL2_RAM_RANGES {
            return Err(Error::RamLen {
                len: vtl2_ram.len() as u64,
                max: MAX_VTL2_RAM_RANGES as u64,
            });
        }

        if !manager.address_space.is_empty() {
            return Err(Error::AlreadyInitialized);
        }

        // The other ranges are reserved, and must overlap with the used range.
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
                RangeWalkResult::Right(typ) => {
                    return Err(Error::ReservedRangeOutsideBootshimUsed {
                        reserved: entry,
                        typ,
                        bootshim_used,
                    });
                }
                RangeWalkResult::Neither => {}
            }
        }

        // Construct the initial state of VTL2 address space by walking ram and reserved ranges
        assert!(manager.address_space.is_empty());
        for (entry, r) in walk_ranges(
            vtl2_ram.iter().map(|e| (e.range, e.vnode)),
            used_ranges.iter().map(|(r, usage)| (*r, usage)),
        ) {
            match r {
                RangeWalkResult::Left(vnode) => {
                    // VTL2 normal ram, unused by anything.
                    manager.address_space.push(AddressRange {
                        range: entry,
                        vnode,
                        usage: AddressUsage::Free,
                    });
                }
                RangeWalkResult::Both(vnode, usage) => {
                    // VTL2 ram, currently in use.
                    manager.address_space.push(AddressRange {
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

        Ok(manager)
    }
}

impl AddressSpaceManager {
    pub const fn new_const() -> Self {
        Self {
            address_space: ArrayVec::new_const(),
            vtl2_pool: false,
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

    /// Allocate a new range of memory with the given type and policy. None is
    /// returned if the allocation was unable to be satisfied.
    ///
    /// `len` is the number of bytes to allocate. The number of bytes are
    /// rounded up to the next 4K page size increment. if `len` is 0, then
    /// `None` is returned.
    ///
    /// `required_vnode` if `Some(u32)` is the vnode to allocate from. If there
    /// are no free ranges left in that vnode, None is returned.
    pub fn allocate(
        &mut self,
        required_vnode: Option<u32>,
        len: u64,
        allocation_type: AllocationType,
        allocation_policy: AllocationPolicy,
    ) -> Option<AllocatedRange> {
        if len == 0 {
            return None;
        }

        // Round up to the next 4k page size, if the caller did not specify a
        // multiple of 4k.
        let len = len.div_ceil(PAGE_SIZE_4K) * PAGE_SIZE_4K;

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

        // Walk ranges in forward/reverse order, depending on allocation policy.
        let index = {
            let iter = self.address_space.iter().enumerate();
            match allocation_policy {
                AllocationPolicy::LowMemory => find_index(iter, required_vnode, len),
                AllocationPolicy::HighMemory => find_index(iter.rev(), required_vnode, len),
            }
        };

        let alloc = index.map(|index| {
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
        });

        if allocation_type == AllocationType::GpaPool && alloc.is_some() {
            self.vtl2_pool = true;
        }

        alloc
    }

    /// Returns an iterator for all VTL2 ranges.
    pub fn vtl2_ranges(&self) -> impl Iterator<Item = (MemoryRange, MemoryVtlType)> + use<'_> {
        memory_range::merge_adjacent_ranges(
            self.address_space.iter().map(|r| (r.range, r.usage.into())),
        )
    }

    /// Returns an iterator for reserved VTL2 ranges that should not be
    /// described as ram to the kernel.
    pub fn reserved_vtl2_ranges(
        &self,
    ) -> impl Iterator<Item = (MemoryRange, ReservedMemoryType)> + use<'_> {
        self.address_space.iter().filter_map(|r| match r.usage {
            AddressUsage::Reserved(typ) => Some((r.range, typ)),
            _ => None,
        })
    }

    /// Returns true if there are VTL2 pool allocations.
    pub fn has_vtl2_pool(&self) -> bool {
        self.vtl2_pool
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let vtl2_ram = &[MemoryEntry {
            range: MemoryRange::new(0x0..0x20000),
            vnode: 0,
            mem_type: MemoryMapEntryType::MEMORY,
        }];

        AddressSpaceManagerBuilder::new(
            &mut address_space,
            vtl2_ram,
            MemoryRange::new(0x0..0xF000),
            [
                MemoryRange::new(0x3000..0x4000),
                MemoryRange::new(0x5000..0x6000),
            ]
            .iter()
            .cloned(),
        )
        .with_reserved_range(MemoryRange::new(0x8000..0xA000))
        .with_sidecar_image(MemoryRange::new(0xA000..0xC000))
        .init()
        .unwrap();

        let range = address_space
            .allocate(
                None,
                0x1000,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1F000..0x20000));
        assert!(address_space.has_vtl2_pool());

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
        let vtl2_ram = &[
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
        ];

        AddressSpaceManagerBuilder::new(
            &mut address_space,
            vtl2_ram,
            MemoryRange::new(0x0..0x10000),
            [
                MemoryRange::new(0x3000..0x4000),
                MemoryRange::new(0x5000..0x6000),
            ]
            .iter()
            .cloned(),
        )
        .with_reserved_range(MemoryRange::new(0x8000..0xA000))
        .with_sidecar_image(MemoryRange::new(0xA000..0xC000))
        .init()
        .unwrap();

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

    // test unaligned 4k allocations
    #[test]
    fn test_unaligned_allocations() {
        let mut address_space = AddressSpaceManager::new_const();
        let vtl2_ram = &[MemoryEntry {
            range: MemoryRange::new(0x0..0x20000),
            vnode: 0,
            mem_type: MemoryMapEntryType::MEMORY,
        }];

        AddressSpaceManagerBuilder::new(
            &mut address_space,
            vtl2_ram,
            MemoryRange::new(0x0..0xF000),
            [
                MemoryRange::new(0x3000..0x4000),
                MemoryRange::new(0x5000..0x6000),
            ]
            .iter()
            .cloned(),
        )
        .with_reserved_range(MemoryRange::new(0x8000..0xA000))
        .with_sidecar_image(MemoryRange::new(0xA000..0xC000))
        .init()
        .unwrap();

        let range = address_space
            .allocate(
                None,
                0x1001,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1E000..0x20000));

        let range = address_space
            .allocate(
                None,
                0xFFF,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
            )
            .unwrap();
        assert_eq!(range.range, MemoryRange::new(0x1D000..0x1E000));

        let range = address_space.allocate(
            None,
            0,
            AllocationType::GpaPool,
            AllocationPolicy::HighMemory,
        );
        assert!(range.is_none());
    }

    // test invalid init ranges
    #[test]
    fn test_invalid_init_ranges() {
        let vtl2_ram = [MemoryEntry {
            range: MemoryRange::new(0x0..0x20000),
            vnode: 0,
            mem_type: MemoryMapEntryType::MEMORY,
        }];
        let bootshim_used = MemoryRange::new(0x0..0xF000);

        // test config range completely outside of bootshim_used
        let mut address_space = AddressSpaceManager::new_const();

        let result = AddressSpaceManagerBuilder::new(
            &mut address_space,
            &vtl2_ram,
            bootshim_used,
            [MemoryRange::new(0x10000..0x11000)].iter().cloned(), // completely outside
        )
        .init();

        assert!(matches!(
            result,
            Err(Error::ReservedRangeOutsideBootshimUsed { .. })
        ));

        // test config range partially overlapping with bootshim_used

        let mut address_space = AddressSpaceManager::new_const();
        let result = AddressSpaceManagerBuilder::new(
            &mut address_space,
            &vtl2_ram,
            bootshim_used,
            [MemoryRange::new(0xE000..0x10000)].iter().cloned(), // partially overlapping
        )
        .init();

        assert!(matches!(
            result,
            Err(Error::ReservedRangeOutsideBootshimUsed { .. })
        ));
    }
}
