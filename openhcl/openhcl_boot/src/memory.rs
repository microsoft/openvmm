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
    /// In-memory bootshim log buffer.
    BootshimLogBuffer,
    /// Persisted state header.
    PersistedStateHeader,
    /// Persisted state payload.
    PersistedStatePayload,
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
            ReservedMemoryType::BootshimLogBuffer => MemoryVtlType::VTL2_BOOTSHIM_LOG_BUFFER,
            ReservedMemoryType::PersistedStateHeader => MemoryVtlType::VTL2_PERSISTED_STATE_HEADER,
            ReservedMemoryType::PersistedStatePayload => {
                MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF
            }
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
    persisted_state_region: MemoryRange,
    vtl2_config: I,
    reserved_range: Option<MemoryRange>,
    sidecar_image: Option<MemoryRange>,
    page_tables: Option<MemoryRange>,
    log_buffer: Option<MemoryRange>,
    pool_range: Option<MemoryRange>,
}

impl<'a, I: Iterator<Item = MemoryRange>> AddressSpaceManagerBuilder<'a, I> {
    /// Create a new builder to initialize an [`AddressSpaceManager`].
    ///
    /// `vtl2_ram` is the list of ram ranges for VTL2, which must be sorted.
    ///
    /// `bootshim_used` is the range used by the bootshim, but may be reclaimed
    /// as ram by the kernel.
    ///
    /// `persisted_state_region` is the range used to store the persisted state
    /// header described by [`loader_defs::shim::PersistedStateHeader`] and
    /// corresponding protbuf payload.
    ///
    /// Other ranges described by other methods must lie within `bootshim_used`.
    pub fn new(
        manager: &'a mut AddressSpaceManager,
        vtl2_ram: &'a [MemoryEntry],
        bootshim_used: MemoryRange,
        persisted_state_region: MemoryRange,
        vtl2_config: I,
    ) -> AddressSpaceManagerBuilder<'a, I> {
        AddressSpaceManagerBuilder {
            manager,
            vtl2_ram,
            bootshim_used,
            persisted_state_region,
            vtl2_config,
            reserved_range: None,
            sidecar_image: None,
            page_tables: None,
            log_buffer: None,
            pool_range: None,
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

    /// Log buffer that is reported as type [`MemoryVtlType::VTL2_BOOTSHIM_LOG_BUFFER`].
    pub fn with_log_buffer(mut self, log_buffer: MemoryRange) -> Self {
        self.log_buffer = Some(log_buffer);
        self
    }

    /// Existing VTL2 GPA pool ranges, reported as type [`MemoryVtlType::VTL2_GPA_POOL`].
    pub fn with_pool_range(mut self, pool_range: MemoryRange) -> Self {
        self.pool_range = Some(pool_range);
        self
    }

    /// Consume the builder and initialize the address space manager.
    pub fn init(self) -> Result<&'a mut AddressSpaceManager, Error> {
        let Self {
            manager,
            vtl2_ram,
            bootshim_used,
            persisted_state_region,
            vtl2_config,
            reserved_range,
            sidecar_image,
            page_tables,
            log_buffer,
            pool_range,
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

        // Split the persisted state region into two: the header which is the
        // first 4K page, and the remainder which is the protobuf payload. Both
        // are reserved ranges.
        let (persisted_header, persisted_payload) =
            persisted_state_region.split_at_offset(PAGE_SIZE_4K);

        // The other ranges are reserved, and must overlap with the used range.
        let mut reserved: ArrayVec<(MemoryRange, ReservedMemoryType), 20> = ArrayVec::new();
        reserved.push((persisted_header, ReservedMemoryType::PersistedStateHeader));
        reserved.push((persisted_payload, ReservedMemoryType::PersistedStatePayload));
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
        reserved.extend(
            log_buffer
                .into_iter()
                .map(|r| (r, ReservedMemoryType::BootshimLogBuffer)),
        );
        reserved.sort_unstable_by_key(|(r, _)| r.start());

        let mut used_ranges: ArrayVec<(MemoryRange, AddressUsage), 13> = ArrayVec::new();

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

        // Add any existing pool range as reserved.
        if let Some(range) = pool_range {
            used_ranges.push((
                range,
                AddressUsage::Reserved(ReservedMemoryType::Vtl2GpaPool),
            ));
            manager.vtl2_pool = true;
        }
        used_ranges.sort_unstable_by_key(|(r, _)| r.start());

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
    ///
    /// Requires that the caller provides a memory range that has room to
    /// be chopped up into an aligned range of length len.
    fn allocate_range(
        &mut self,
        index: usize,
        len: u64,
        usage: AddressUsage,
        allocation_policy: AllocationPolicy,
        alignment: Option<u64>,
    ) -> AllocatedRange {
        assert!(usage != AddressUsage::Free);
        let range = self.address_space.get_mut(index).expect("valid index");
        assert_eq!(range.usage, AddressUsage::Free);

        let subrange = if let Some(alignment) = alignment {
            range.range.aligned_subrange(alignment)
        } else {
            range.range
        };

        assert!(subrange.len() >= len);
        assert_ne!(subrange, MemoryRange::EMPTY);

        let used = match allocation_policy {
            AllocationPolicy::LowMemory => {
                // Allocate from the beginning (low addresses)
                let (used, _) = subrange.split_at_offset(len);
                used
            }
            AllocationPolicy::HighMemory => {
                // Allocate from the end (high addresses)
                let offset = subrange.len() - len;
                let (_, used) = subrange.split_at_offset(offset);
                used
            }
        };

        let left = MemoryRange::new(range.range.start()..used.start());
        let right = MemoryRange::new(used.end()..range.range.end());

        let to_address_range = |r: MemoryRange| -> Option<AddressRange> {
            if !r.is_empty() {
                Some(AddressRange {
                    range: r,
                    vnode: range.vnode,
                    usage: AddressUsage::Free,
                })
            } else {
                None
            }
        };

        let left = to_address_range(left);
        let right = to_address_range(right);

        // Update this range to mark it as used
        range.usage = usage;
        range.range = used;
        let allocated = AllocatedRange {
            range: used,
            vnode: range.vnode,
        };

        if let Some(right) = right {
            self.address_space.insert(index + 1, right);
        }

        if let Some(left) = left {
            self.address_space.insert(index, left);
        }

        allocated
    }

    fn allocate_inner(
        &mut self,
        required_vnode: Option<u32>,
        len: u64,
        allocation_type: AllocationType,
        allocation_policy: AllocationPolicy,
        alignment: Option<u64>,
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
            alignment: Option<u64>,
        ) -> Option<usize> {
            iter.find_map(|(index, range)| {
                let is_aligned: bool = alignment.is_none()
                    || (alignment.is_some()
                        && range.range.aligned_subrange(alignment.unwrap()).len() >= len);
                if range.usage == AddressUsage::Free
                    && range.range.len() >= len
                    && preferred_vnode.map(|pv| pv == range.vnode).unwrap_or(true)
                    && is_aligned
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
                AllocationPolicy::LowMemory => find_index(iter, required_vnode, len, alignment),
                AllocationPolicy::HighMemory => {
                    find_index(iter.rev(), required_vnode, len, alignment)
                }
            }
        };

        let address_usage = match allocation_type {
            AllocationType::GpaPool => AddressUsage::Reserved(ReservedMemoryType::Vtl2GpaPool),
            AllocationType::SidecarNode => AddressUsage::Reserved(ReservedMemoryType::SidecarNode),
            AllocationType::TdxPageTables => {
                AddressUsage::Reserved(ReservedMemoryType::TdxPageTables)
            }
        };

        let alloc = index.map(|index| {
            self.allocate_range(index, len, address_usage, allocation_policy, alignment)
        });

        if allocation_type == AllocationType::GpaPool && alloc.is_some() {
            self.vtl2_pool = true;
        }

        alloc
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
        self.allocate_inner(
            required_vnode,
            len,
            allocation_type,
            allocation_policy,
            None,
        )
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
    ///
    /// `alignment` aligns the top of HighMemory allocations to `alignment`
    /// bytes, and aligns the bottom of LowMemory allocations
    #[cfg_attr(all(target_arch = "aarch64", not(test)), expect(dead_code))]
    pub fn allocate_aligned(
        &mut self,
        required_vnode: Option<u32>,
        len: u64,
        allocation_type: AllocationType,
        allocation_policy: AllocationPolicy,
        alignment: u64,
    ) -> Option<AllocatedRange> {
        self.allocate_inner(
            required_vnode,
            len,
            allocation_type,
            allocation_policy,
            Some(alignment),
        )
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

    /// Returns an iterator for all the free ranges for the given numa node.
    /// This is used for diagnostics and debugging purposes.
    pub fn free_ranges(&self, vnode: u32) -> impl Iterator<Item = MemoryRange> + use<'_> {
        self.address_space.iter().filter_map(move |r| {
            if r.usage == AddressUsage::Free && r.vnode == vnode {
                Some(r.range)
            } else {
                None
            }
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
    #[cfg_attr(target_arch = "aarch64", expect(dead_code))]
    TdxPageTables,
}

pub enum AllocationPolicy {
    // prefer low memory
    LowMemory,
    // prefer high memory
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
            MemoryRange::new(0x0..0x2000),
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

    #[test]
    fn test_allocate_aligned() {
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
            MemoryRange::new(0x0..0x2000),
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

        let alignment = 4096 * 16;
        let range = address_space
            .allocate_aligned(
                None,
                0x1000,
                AllocationType::GpaPool,
                AllocationPolicy::LowMemory,
                alignment,
            )
            .unwrap();

        assert_eq!(0, range.range.start() % alignment);

        let alignment = 4096 * 4;
        let range = address_space
            .allocate_aligned(
                None,
                0x1000,
                AllocationType::GpaPool,
                AllocationPolicy::HighMemory,
                alignment,
            )
            .unwrap();

        assert_eq!(0, range.range.end() % alignment);
    }

    #[test]
    fn test_failed_alignment() {
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
            MemoryRange::new(0x0..0x2000),
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

        let alignment = 1024 * 1024 * 2;
        let range = address_space.allocate_aligned(
            None,
            0x1000,
            AllocationType::GpaPool,
            AllocationPolicy::LowMemory,
            alignment,
        );
        assert!(range.is_none());
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
            MemoryRange::new(0x0..0x2000),
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
            MemoryRange::new(0x0..0x2000),
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
            MemoryRange::new(0x0..0x2000),
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
            MemoryRange::new(0x0..0x2000),
            [MemoryRange::new(0xE000..0x10000)].iter().cloned(), // partially overlapping
        )
        .init();

        assert!(matches!(
            result,
            Err(Error::ReservedRangeOutsideBootshimUsed { .. })
        ));

        // test persisted region outside of bootshim_used
        let mut address_space = AddressSpaceManager::new_const();
        let result = AddressSpaceManagerBuilder::new(
            &mut address_space,
            &vtl2_ram,
            bootshim_used,
            MemoryRange::new(0x10000..0x14000), // outside
            [MemoryRange::new(0xE000..0xF000)].iter().cloned(),
        )
        .init();

        assert!(matches!(
            result,
            Err(Error::ReservedRangeOutsideBootshimUsed { .. })
        ));
    }

    #[test]
    fn test_persisted_range() {
        let vtl2_ram = [MemoryEntry {
            range: MemoryRange::new(0x0..0x20000),
            vnode: 0,
            mem_type: MemoryMapEntryType::MEMORY,
        }];
        let bootshim_used = MemoryRange::new(0x0..0xF000);

        let mut address_space = AddressSpaceManager::new_const();
        AddressSpaceManagerBuilder::new(
            &mut address_space,
            &vtl2_ram,
            bootshim_used,
            MemoryRange::new(0x0..0xE000),
            [MemoryRange::new(0xE000..0xF000)].iter().cloned(),
        )
        .init()
        .unwrap();

        let expected = [
            (
                MemoryRange::new(0x0..0x1000),
                MemoryVtlType::VTL2_PERSISTED_STATE_HEADER,
            ),
            (
                MemoryRange::new(0x1000..0xE000),
                MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF,
            ),
            (MemoryRange::new(0xE000..0xF000), MemoryVtlType::VTL2_CONFIG),
            (MemoryRange::new(0xF000..0x20000), MemoryVtlType::VTL2_RAM),
        ];

        for (expected, actual) in expected.iter().zip(address_space.vtl2_ranges()) {
            assert_eq!(*expected, actual);
        }

        // test with free space between state region and config
        let mut address_space = AddressSpaceManager::new_const();
        AddressSpaceManagerBuilder::new(
            &mut address_space,
            &vtl2_ram,
            bootshim_used,
            MemoryRange::new(0x0..0xA000),
            [MemoryRange::new(0xE000..0xF000)].iter().cloned(),
        )
        .init()
        .unwrap();

        let expected = [
            (
                MemoryRange::new(0x0..0x1000),
                MemoryVtlType::VTL2_PERSISTED_STATE_HEADER,
            ),
            (
                MemoryRange::new(0x1000..0xA000),
                MemoryVtlType::VTL2_PERSISTED_STATE_PROTOBUF,
            ),
            (MemoryRange::new(0xA000..0xE000), MemoryVtlType::VTL2_RAM),
            (MemoryRange::new(0xE000..0xF000), MemoryVtlType::VTL2_CONFIG),
            (MemoryRange::new(0xF000..0x20000), MemoryVtlType::VTL2_RAM),
        ];

        for (expected, actual) in expected.iter().zip(address_space.vtl2_ranges()) {
            assert_eq!(*expected, actual);
        }
    }

    // FIXME: test pool ranges
}
