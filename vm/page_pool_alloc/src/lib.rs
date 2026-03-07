// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements a page memory allocator for allocating pages from a
//! given portion of the guest address space.

mod device_dma;

pub use device_dma::PagePoolDmaBuffer;

use anyhow::Context;
use inspect::Inspect;
use inspect::Response;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use safeatomic::AtomicSliceOps;
use sparse_mmap::Mappable;
use sparse_mmap::MappableRef;
use sparse_mmap::SparseMapping;
use sparse_mmap::alloc_shared_memory;
use std::fmt::Debug;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::sync::atomic::AtomicU8;
use thiserror::Error;

const PAGE_SIZE: u64 = 4096;

/// Save restore suport for [`PagePool`].
pub mod save_restore {
    use super::PAGE_SIZE;
    use super::PagePool;
    use super::Slot;
    use super::SlotState;
    use crate::ResolvedSlotState;
    use memory_range::MemoryRange;
    use mesh::payload::Protobuf;
    use vmcore::save_restore::SaveRestore;
    use vmcore::save_restore::SavedStateRoot;

    #[derive(Protobuf)]
    #[mesh(package = "openvmm.pagepool")]
    enum InnerSlotState {
        #[mesh(1)]
        Free,
        #[mesh(2)]
        Allocated {
            #[mesh(1)]
            device_id: String,
            #[mesh(2)]
            tag: String,
        },
        #[mesh(3)]
        Leaked {
            #[mesh(1)]
            device_id: String,
            #[mesh(2)]
            tag: String,
        },
    }

    #[derive(Protobuf)]
    #[mesh(package = "openvmm.pagepool")]
    struct SlotSavedState {
        #[mesh(1)]
        base_pfn: u64,
        #[mesh(2)]
        size_pages: u64,
        #[mesh(3)]
        state: InnerSlotState,
    }

    /// The saved state for [`PagePool`].
    #[derive(Protobuf, SavedStateRoot)]
    #[mesh(package = "openvmm.pagepool")]
    pub struct PagePoolState {
        #[mesh(1)]
        state: Vec<SlotSavedState>,
        #[mesh(2)]
        ranges: Vec<MemoryRange>,
    }

    impl SaveRestore for PagePool {
        type SavedState = PagePoolState;

        fn save(&mut self) -> Result<Self::SavedState, vmcore::save_restore::SaveError> {
            let state = self.inner.state.lock();
            Ok(PagePoolState {
                state: state
                    .slots
                    .iter()
                    .map(|slot| {
                        let slot = slot.resolve(&state.device_ids);
                        let inner_state = match slot.state {
                            ResolvedSlotState::Free => InnerSlotState::Free,
                            ResolvedSlotState::Allocated { device_id, tag } => {
                                InnerSlotState::Allocated {
                                    device_id: device_id.to_string(),
                                    tag: tag.to_string(),
                                }
                            }
                            ResolvedSlotState::Leaked { device_id, tag } => {
                                InnerSlotState::Leaked {
                                    device_id: device_id.to_string(),
                                    tag: tag.to_string(),
                                }
                            }
                            ResolvedSlotState::AllocatedPendingRestore { .. } => {
                                panic!("should not save allocated pending restore")
                            }
                        };

                        SlotSavedState {
                            base_pfn: slot.base_pfn,
                            size_pages: slot.size_pages,
                            state: inner_state,
                        }
                    })
                    .collect(),
                ranges: self.ranges.clone(),
            })
        }

        fn restore(
            &mut self,
            mut state: Self::SavedState,
        ) -> Result<(), vmcore::save_restore::RestoreError> {
            // Verify that the pool describes the same regions of memory as the
            // saved state.
            for (current, saved) in self.ranges.iter().zip(state.ranges.iter()) {
                if current != saved {
                    // TODO: return unmatched range or vecs?
                    return Err(vmcore::save_restore::RestoreError::InvalidSavedState(
                        anyhow::anyhow!("pool ranges do not match"),
                    ));
                }
            }

            let mut inner = self.inner.state.lock();

            // Verify there are no existing allocators present, as we rely on
            // the pool being completely free since we will overwrite the state
            // of the pool with the stored slot info.
            //
            // Note that this also means that the pool does not have any pending
            // allocations, as it's impossible to allocate without creating an
            // allocator.
            if !inner.device_ids.is_empty() {
                return Err(vmcore::save_restore::RestoreError::InvalidSavedState(
                    anyhow::anyhow!("existing allocators present, pool must be empty to restore"),
                ));
            }

            state.state.sort_by_key(|slot| slot.base_pfn);

            let mut mapping_offset = 0;
            inner.slots = state
                .state
                .into_iter()
                .map(|slot| {
                    let inner = match slot.state {
                        InnerSlotState::Free => SlotState::Free,
                        InnerSlotState::Allocated { device_id, tag } => {
                            SlotState::AllocatedPendingRestore { device_id, tag }
                        }
                        InnerSlotState::Leaked { device_id, tag } => {
                            SlotState::Leaked { device_id, tag }
                        }
                    };

                    let slot = Slot {
                        base_pfn: slot.base_pfn,
                        mapping_offset: mapping_offset as usize,
                        size_pages: slot.size_pages,
                        state: inner,
                    };
                    mapping_offset += slot.size_pages * PAGE_SIZE;
                    slot
                })
                .collect();

            if mapping_offset != self.inner.mapping.len() as u64 {
                return Err(vmcore::save_restore::RestoreError::InvalidSavedState(
                    anyhow::anyhow!("missing slots in saved state"),
                ));
            }

            Ok(())
        }
    }
}

/// Errors returned on allocation methods.
#[derive(Debug, Error)]
pub enum Error {
    /// Unable to allocate memory due to not enough free pages.
    #[error("unable to allocate page pool size {size} with tag {tag}")]
    PagePoolOutOfMemory {
        /// The size in pages of the allocation.
        size: u64,
        /// The tag of the allocation.
        tag: String,
    },
    /// Unable to create mapping requested for the allocation.
    #[error("failed to create mapping for allocation")]
    Mapping(#[source] anyhow::Error),
    /// No matching allocation found for restore.
    #[error("no matching allocation found for restore")]
    NoMatchingAllocation,
}

/// Error returned when unrestored allocations are found.
#[derive(Debug, Error)]
#[error("unrestored allocations found")]
pub struct UnrestoredAllocations;

#[derive(Debug, PartialEq, Eq)]
struct Slot {
    base_pfn: u64,
    mapping_offset: usize,
    size_pages: u64,
    state: SlotState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SlotState {
    Free,
    Allocated {
        /// This is an index into the outer [`PagePoolInner`]'s device_ids
        /// vector.
        device_id: usize,
        tag: String,
    },
    /// This allocation was restored, and is waiting for a
    /// [`PagePoolAllocator::restore_alloc`] to restore it.
    AllocatedPendingRestore {
        device_id: String,
        tag: String,
    },
    /// This allocation was leaked, and is no longer able to be allocated from.
    Leaked {
        device_id: String,
        tag: String,
    },
}

impl Slot {
    fn resolve<'a>(&'a self, device_ids: &'a [DeviceId]) -> ResolvedSlot<'a> {
        ResolvedSlot {
            base_pfn: self.base_pfn,
            mapping_offset: self.mapping_offset,
            size_pages: self.size_pages,
            state: match self.state {
                SlotState::Free => ResolvedSlotState::Free,
                SlotState::Allocated { device_id, ref tag } => ResolvedSlotState::Allocated {
                    device_id: device_ids[device_id].name(),
                    tag,
                },
                SlotState::AllocatedPendingRestore {
                    ref device_id,
                    ref tag,
                } => ResolvedSlotState::AllocatedPendingRestore { device_id, tag },
                SlotState::Leaked {
                    ref device_id,
                    ref tag,
                } => ResolvedSlotState::Leaked { device_id, tag },
            },
        }
    }
}

impl SlotState {
    fn restore_allocated(&mut self, device_id: usize) {
        if !matches!(self, SlotState::AllocatedPendingRestore { .. }) {
            panic!("invalid state");
        }

        // Temporarily swap with free so we can move the string tag to the
        // restored state without allocating.
        let prev = std::mem::replace(self, SlotState::Free);
        *self = match prev {
            SlotState::AllocatedPendingRestore { device_id: _, tag } => {
                SlotState::Allocated { device_id, tag }
            }
            _ => unreachable!(),
        };
    }
}

#[derive(Inspect)]
struct ResolvedSlot<'a> {
    base_pfn: u64,
    mapping_offset: usize,
    size_pages: u64,
    state: ResolvedSlotState<'a>,
}

#[derive(Inspect)]
#[inspect(external_tag)]
enum ResolvedSlotState<'a> {
    Free,
    Allocated { device_id: &'a str, tag: &'a str },
    AllocatedPendingRestore { device_id: &'a str, tag: &'a str },
    Leaked { device_id: &'a str, tag: &'a str },
}

#[derive(Inspect, Debug, Clone, PartialEq, Eq)]
#[inspect(tag = "state")]
enum DeviceId {
    /// A device id that is in use by an allocator.
    Used(#[inspect(rename = "name")] String),
    /// A device id that was dropped and can be reused if an allocator with the
    /// same name is created.
    Unassigned(#[inspect(rename = "name")] String),
}

impl DeviceId {
    fn name(&self) -> &str {
        match self {
            DeviceId::Used(name) => name,
            DeviceId::Unassigned(name) => name,
        }
    }
}

#[derive(Inspect)]
struct PagePoolInner {
    #[inspect(flatten)]
    state: Mutex<PagePoolState>,
    /// The pfn_bias for the pool.
    pfn_bias: u64,
    /// The mapper used to create mappings for allocations.
    source: Box<dyn PoolSource>,
    #[inspect(skip)]
    mapping: SparseMapping,
}

impl Debug for PagePoolInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PagePoolInner")
            .field("state", &self.state)
            .field("pfn_bias", &self.pfn_bias)
            .field("mapping", &self.mapping)
            .finish()
    }
}

#[derive(Debug)]
struct PagePoolState {
    /// The internal slots for the pool, representing page state.
    slots: Vec<Slot>,
    /// The list of device ids for outstanding allocators. Each name must be
    /// unique.
    device_ids: Vec<DeviceId>,
}

impl Inspect for PagePoolState {
    fn inspect(&self, req: inspect::Request<'_>) {
        let Self { slots, device_ids } = self;
        req.respond().field(
            "slots",
            inspect::iter_by_index(slots).map_value(|s| s.resolve(device_ids)),
        );
    }
}

/// A handle for a page pool allocation. When dropped, the allocation is
/// freed.
#[derive(Debug)]
pub struct PagePoolHandle {
    inner: Arc<PagePoolInner>,
    base_pfn: u64,
    size_pages: u64,
    mapping_offset: usize,
}

impl PagePoolHandle {
    /// The base pfn (with bias) for this allocation.
    pub fn base_pfn(&self) -> u64 {
        self.base_pfn + self.inner.pfn_bias
    }

    /// The base pfn without bias for this allocation.
    pub fn base_pfn_without_bias(&self) -> u64 {
        self.base_pfn
    }

    /// The number of 4K pages for this allocation.
    pub fn size_pages(&self) -> u64 {
        self.size_pages
    }

    /// The associated mapping with this allocation.
    pub fn mapping(&self) -> &[AtomicU8] {
        self.inner
            .mapping
            .atomic_slice(self.mapping_offset, (self.size_pages * PAGE_SIZE) as usize)
    }

    /// Create a memory block from this allocation.
    fn into_memory_block(self) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        let pfns: Vec<_> = (self.base_pfn()..self.base_pfn() + self.size_pages).collect();
        Ok(user_driver::memory::MemoryBlock::new(PagePoolDmaBuffer {
            alloc: self,
            pfns,
        }))
    }
}

impl Drop for PagePoolHandle {
    fn drop(&mut self) {
        let mut inner = self.inner.state.lock();

        let slot = inner
            .slots
            .iter_mut()
            .find(|slot| {
                if matches!(slot.state, SlotState::Allocated { .. }) {
                    slot.base_pfn == self.base_pfn && slot.size_pages == self.size_pages
                } else {
                    false
                }
            })
            .expect("must find allocation");

        assert_eq!(slot.mapping_offset, self.mapping_offset);
        slot.state = SlotState::Free;
    }
}

/// A source for pool allocations.
pub trait PoolSource: Inspect + Send + Sync {
    /// The bias to apply to the physical address of each allocation.
    fn address_bias(&self) -> u64;
    /// Translates a physical address into the file offset to use when mapping
    /// the page.
    fn file_offset(&self, address: u64) -> u64;
    /// Returns the OS object to map pages from.
    fn mappable(&self) -> MappableRef<'_>;
}

/// A mapper that uses an internal buffer to map pages. This is meant to be used
/// for tests that use [`PagePool`].
#[derive(Inspect)]
#[inspect(extra = "TestMapper::inspect_extra")]
pub struct TestMapper {
    #[inspect(skip)]
    mem: Mappable,
    len: usize,
}

impl TestMapper {
    /// Create a new test mapper that holds an internal buffer of `size_pages`.
    pub fn new(size_pages: u64) -> anyhow::Result<Self> {
        let len = (size_pages * PAGE_SIZE) as usize;
        let fd = alloc_shared_memory(len).context("creating shared mem")?;

        Ok(Self { mem: fd, len })
    }

    /// Returns [`SparseMapping`] that maps starting at page 0.
    pub fn sparse_mapping(&self) -> SparseMapping {
        let mappable = self.mappable();
        let mapping = SparseMapping::new(self.len).unwrap();
        mapping.map_file(0, self.len, mappable, 0, true).unwrap();
        mapping
    }

    fn inspect_extra(&self, resp: &mut Response<'_>) {
        resp.field("type", "test");
    }
}

impl PoolSource for TestMapper {
    fn address_bias(&self) -> u64 {
        0
    }

    fn file_offset(&self, address: u64) -> u64 {
        address
    }

    fn mappable(&self) -> MappableRef<'_> {
        #[cfg(windows)]
        return std::os::windows::io::AsHandle::as_handle(&self.mem);
        #[cfg(not(windows))]
        std::os::unix::io::AsFd::as_fd(&self.mem)
    }
}

/// A page allocator for memory.
///
/// This memory may be private memory, or shared visibility memory on isolated
/// VMs. depending on the memory range passed into the corresponding new
/// methods.
///
/// Pages are allocated via [`PagePoolAllocator`] from [`Self::allocator`] or
/// [`PagePoolAllocatorSpawner::allocator`].
///
/// This struct is considered the "owner" of the pool allowing for save/restore.
#[derive(Inspect)]
pub struct PagePool {
    #[inspect(flatten)]
    inner: Arc<PagePoolInner>,
    #[inspect(iter_by_index)]
    ranges: Vec<MemoryRange>,
}

impl PagePool {
    /// Returns a new page pool managing the address ranges in `ranges`,
    /// using `source` to access the memory.
    pub fn new<T: PoolSource + 'static>(ranges: &[MemoryRange], source: T) -> anyhow::Result<Self> {
        Self::new_internal(ranges, Box::new(source))
    }

    fn new_internal(memory: &[MemoryRange], source: Box<dyn PoolSource>) -> anyhow::Result<Self> {
        let mut mapping_offset = 0;
        let pages = memory
            .iter()
            .map(|range| {
                let slot = Slot {
                    base_pfn: range.start() / PAGE_SIZE,
                    size_pages: range.len() / PAGE_SIZE,
                    mapping_offset,
                    state: SlotState::Free,
                };
                mapping_offset += range.len() as usize;
                slot
            })
            .collect();

        let total_len = mapping_offset;

        // Create a contiguous mapping of the memory ranges.
        let mapping = SparseMapping::new(total_len).context("failed to reserve VA")?;
        let mappable = source.mappable();
        let mut mapping_offset = 0;
        for range in memory {
            let file_offset = source.file_offset(range.start());
            let len = range.len() as usize;
            mapping
                .map_file(mapping_offset, len, mappable, file_offset, true)
                .context("failed to map range")?;
            mapping_offset += len;
        }

        assert_eq!(mapping_offset, total_len);

        Ok(Self {
            inner: Arc::new(PagePoolInner {
                state: Mutex::new(PagePoolState {
                    slots: pages,
                    device_ids: Vec::new(),
                }),
                pfn_bias: source.address_bias() / PAGE_SIZE,
                source,
                mapping,
            }),
            ranges: memory.to_vec(),
        })
    }

    /// Create an allocator instance that can be used to allocate pages. The
    /// specified `device_name` must be unique.
    ///
    /// Users should create a new allocator for each device, as the device name
    /// is used to track allocations in the pool.
    pub fn allocator(&self, device_name: String) -> anyhow::Result<PagePoolAllocator> {
        PagePoolAllocator::new(&self.inner, device_name)
    }

    /// Create a spawner that allows creating multiple allocators.
    pub fn allocator_spawner(&self) -> PagePoolAllocatorSpawner {
        PagePoolAllocatorSpawner {
            inner: self.inner.clone(),
        }
    }

    /// Validate that all allocations have been restored. This should be called
    /// after all devices have been restored.
    ///
    /// `leak_unrestored` controls what to do if a matching allocation was not
    /// restored. If true, the allocation is marked as leaked and the function
    /// returns Ok. If false, the function returns an error if any are
    /// unmatched.
    ///
    /// Unmatched allocations are always logged via a `tracing::warn!` log.
    pub fn validate_restore(&self, leak_unrestored: bool) -> Result<(), UnrestoredAllocations> {
        let mut inner = self.inner.state.lock();
        let mut unrestored_allocation = false;

        // Mark unrestored allocations as leaked.
        for slot in inner.slots.iter_mut() {
            match &slot.state {
                SlotState::Free | SlotState::Allocated { .. } | SlotState::Leaked { .. } => {}
                SlotState::AllocatedPendingRestore { device_id, tag } => {
                    tracing::warn!(
                        base_pfn = slot.base_pfn,
                        pfn_bias = self.inner.pfn_bias,
                        size_pages = slot.size_pages,
                        device_id = device_id,
                        tag = tag.as_str(),
                        "unrestored allocation"
                    );

                    if leak_unrestored {
                        slot.state = SlotState::Leaked {
                            device_id: device_id.clone(),
                            tag: tag.clone(),
                        };
                    }

                    unrestored_allocation = true;
                }
            }
        }

        if unrestored_allocation && !leak_unrestored {
            Err(UnrestoredAllocations)
        } else {
            Ok(())
        }
    }
}

/// A spawner for [`PagePoolAllocator`] instances.
///
/// Useful when you need to create multiple allocators, without having ownership
/// of the actual [`PagePool`].
pub struct PagePoolAllocatorSpawner {
    inner: Arc<PagePoolInner>,
}

impl PagePoolAllocatorSpawner {
    /// Create an allocator instance that can be used to allocate pages. The
    /// specified `device_name` must be unique.
    ///
    /// Users should create a new allocator for each device, as the device name
    /// is used to track allocations in the pool.
    pub fn allocator(&self, device_name: String) -> anyhow::Result<PagePoolAllocator> {
        PagePoolAllocator::new(&self.inner, device_name)
    }
}

/// A page allocator for memory.
///
/// Pages are allocated via the [`Self::alloc`] method and freed by dropping the
/// associated handle returned.
///
/// When an allocator is dropped, outstanding allocations for that device
/// are left as-is in the pool. A new allocator can then be created with the
/// same name. Exisitng allocations with that same device_name will be
/// linked to the new allocator.
#[derive(Inspect)]
pub struct PagePoolAllocator {
    #[inspect(skip)]
    inner: Arc<PagePoolInner>,
    #[inspect(skip)]
    device_id: usize,
}

impl PagePoolAllocator {
    fn new(inner: &Arc<PagePoolInner>, device_name: String) -> anyhow::Result<Self> {
        let device_id;
        {
            let mut inner = inner.state.lock();

            let index = inner
                .device_ids
                .iter()
                .position(|id| id.name() == device_name);

            // Device ID must be unique, or be unassigned or pending a restore.
            match index {
                Some(index) => {
                    let entry = &mut inner.device_ids[index];

                    match entry {
                        DeviceId::Unassigned(_) => {
                            *entry = DeviceId::Used(device_name);
                            device_id = index;
                        }
                        DeviceId::Used(_) => {
                            anyhow::bail!("device name {device_name} already in use");
                        }
                    }
                }
                None => {
                    inner.device_ids.push(DeviceId::Used(device_name));
                    device_id = inner.device_ids.len() - 1;
                }
            }
        }

        Ok(Self {
            inner: inner.clone(),
            device_id,
        })
    }

    fn alloc_inner(&self, size_pages: NonZeroU64, tag: String) -> Result<PagePoolHandle, Error> {
        let mut inner = self.inner.state.lock();
        let size_pages = size_pages.get();

        let index = inner
            .slots
            .iter()
            .position(|slot| match slot.state {
                SlotState::Free => slot.size_pages >= size_pages,
                SlotState::Allocated { .. }
                | SlotState::AllocatedPendingRestore { .. }
                | SlotState::Leaked { .. } => false,
            })
            .ok_or(Error::PagePoolOutOfMemory {
                size: size_pages,
                tag: tag.clone(),
            })?;

        // Track which slots we should append if the mapping creation succeeds.
        // If the mapping creation fails, we instead commit the original free
        // slot back to the pool.
        let (allocation_slot, free_slot) = {
            let slot = inner.slots.swap_remove(index);
            assert!(matches!(slot.state, SlotState::Free));

            let allocation_slot = Slot {
                base_pfn: slot.base_pfn,
                mapping_offset: slot.mapping_offset,
                size_pages,
                state: SlotState::Allocated {
                    device_id: self.device_id,
                    tag: tag.clone(),
                },
            };

            let free_slot = if slot.size_pages > size_pages {
                Some(Slot {
                    base_pfn: slot.base_pfn + size_pages,
                    mapping_offset: slot.mapping_offset + (size_pages * PAGE_SIZE) as usize,
                    size_pages: slot.size_pages - size_pages,
                    state: SlotState::Free,
                })
            } else {
                None
            };

            (allocation_slot, free_slot)
        };

        let base_pfn = allocation_slot.base_pfn;
        let mapping_offset = allocation_slot.mapping_offset;
        assert_eq!(mapping_offset % PAGE_SIZE as usize, 0);

        // Commit state to the pool.
        inner.slots.push(allocation_slot);
        if let Some(free_slot) = free_slot {
            inner.slots.push(free_slot);
        }

        Ok(PagePoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            size_pages,
            mapping_offset,
        })
    }

    /// Allocate contiguous pages from the page pool with the given tag. If a
    /// contiguous region of free pages is not available, then an error is
    /// returned.
    pub fn alloc(&self, size_pages: NonZeroU64, tag: String) -> Result<PagePoolHandle, Error> {
        self.alloc_inner(size_pages, tag)
    }

    /// Restore an allocation that was previously allocated in the pool. The
    /// base_pfn, size_pages, and device must match.
    ///
    /// `with_mapping` specifies if a mapping should be created that can be used
    /// via [`PagePoolHandle::mapping`].
    pub fn restore_alloc(
        &self,
        base_pfn: u64,
        size_pages: NonZeroU64,
    ) -> Result<PagePoolHandle, Error> {
        let size_pages = size_pages.get();
        let mut inner = self.inner.state.lock();
        let inner = &mut *inner;
        let slot = inner
            .slots
            .iter_mut()
            .find(|slot| {
                if let SlotState::AllocatedPendingRestore { device_id, tag: _ } = &slot.state {
                    device_id == inner.device_ids[self.device_id].name()
                        && slot.base_pfn == base_pfn
                        && slot.size_pages == size_pages
                } else {
                    false
                }
            })
            .ok_or(Error::NoMatchingAllocation)?;

        slot.state.restore_allocated(self.device_id);
        assert_eq!(slot.mapping_offset % PAGE_SIZE as usize, 0);

        Ok(PagePoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            size_pages,
            mapping_offset: slot.mapping_offset,
        })
    }

    /// Restore all pending allocs
    pub fn restore_pending_allocs(&self) -> Vec<PagePoolHandle> {
        let mut inner = self.inner.state.lock();
        let inner = &mut *inner;
        let mut slots: Vec<&mut Slot> = inner
            .slots
            .iter_mut()
            .filter(|slot| {
                if let SlotState::AllocatedPendingRestore {
                    device_id: slot_device_id,
                    tag: _,
                } = &slot.state
                {
                    return inner.device_ids[self.device_id].name() == *slot_device_id;
                }
                false
            })
            .collect();

        slots
            .iter_mut()
            .map(|slot| {
                slot.state.restore_allocated(self.device_id);
                PagePoolHandle {
                    inner: self.inner.clone(),
                    base_pfn: slot.base_pfn,
                    size_pages: slot.size_pages,
                    mapping_offset: slot.mapping_offset,
                }
            })
            .collect()
    }
}

impl Drop for PagePoolAllocator {
    fn drop(&mut self) {
        let mut inner = self.inner.state.lock();
        let device_name = inner.device_ids[self.device_id].name().to_string();
        let prev = std::mem::replace(
            &mut inner.device_ids[self.device_id],
            DeviceId::Unassigned(device_name),
        );
        assert!(matches!(prev, DeviceId::Used(_)));
    }
}

impl user_driver::DmaClient for PagePoolAllocator {
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        if !(len as u64).is_multiple_of(PAGE_SIZE) {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = NonZeroU64::new(len as u64 / PAGE_SIZE)
            .context("allocation of size 0 not supported")?;

        let alloc = self
            .alloc(size_pages, "vfio dma".into())
            .context("failed to allocate shared mem")?;

        // The VfioDmaBuffer trait requires that newly allocated buffers are
        // zeroed.
        alloc.mapping().atomic_fill(0);
        alloc.into_memory_block()
    }

    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>> {
        let allocs = self.restore_pending_allocs();

        allocs
            .into_iter()
            .map(|alloc| alloc.into_memory_block())
            .collect()
    }
}

#[cfg(test)]
mod test {
    use crate::PAGE_SIZE;
    use crate::PagePool;
    use crate::PoolSource;
    use crate::TestMapper;
    use inspect::Inspect;
    use memory_range::MemoryRange;
    use safeatomic::AtomicSliceOps;
    use sparse_mmap::MappableRef;
    use vmcore::save_restore::SaveRestore;

    #[derive(Inspect)]
    #[inspect(bound = "T: Inspect")]
    struct BiasedMapper<T> {
        mapper: T,
        bias: u64,
    }

    impl<T: PoolSource> BiasedMapper<T> {
        fn new(mapper: T, bias: u64) -> Self {
            Self { mapper, bias }
        }
    }

    impl<T: PoolSource> PoolSource for BiasedMapper<T> {
        fn address_bias(&self) -> u64 {
            self.bias.wrapping_add(self.mapper.address_bias())
        }

        fn file_offset(&self, address: u64) -> u64 {
            self.mapper.file_offset(address)
        }

        fn mappable(&self) -> MappableRef<'_> {
            self.mapper.mappable()
        }
    }

    fn big_test_mapper() -> TestMapper {
        TestMapper::new(1024 * 1024).unwrap()
    }

    #[test]
    fn test_basic_alloc() {
        let pfn_bias = 15;
        let pool = PagePool::new(
            &[MemoryRange::from_4k_gpn_range(10..30)],
            BiasedMapper::new(big_test_mapper(), pfn_bias * PAGE_SIZE),
        )
        .unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        assert_eq!(a1.base_pfn, 10);
        assert_eq!(a1.base_pfn(), a1.base_pfn + pfn_bias);
        assert_eq!(a1.base_pfn_without_bias(), a1.base_pfn);
        assert_eq!(a1.size_pages, 5);

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        assert_eq!(a2.base_pfn, 15);
        assert_eq!(a2.base_pfn(), a2.base_pfn + pfn_bias);
        assert_eq!(a2.base_pfn_without_bias(), a2.base_pfn);
        assert_eq!(a2.size_pages, 15);

        assert!(alloc.alloc(1.try_into().unwrap(), "failed".into()).is_err());

        drop(a1);
        drop(a2);

        let inner = alloc.inner.state.lock();
        assert_eq!(inner.slots.len(), 2);
    }

    #[test]
    fn test_duplicate_device_name() {
        let pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        let _alloc = pool.allocator("test".into()).unwrap();

        assert!(pool.allocator("test".into()).is_err());
    }

    #[test]
    fn test_dropping_allocator() {
        let pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..40)], big_test_mapper()).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();
        let _alloc2 = pool.allocator("test2".into()).unwrap();

        let _a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        let _a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();

        drop(alloc);

        let alloc = pool.allocator("test".into()).unwrap();
        let _a3 = alloc.alloc(5.try_into().unwrap(), "alloc3".into()).unwrap();
    }

    #[test]
    fn test_save_restore() {
        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        let a1_pfn = a1.base_pfn();
        let a1_size = a1.size_pages;

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        let a2_pfn = a2.base_pfn();
        let a2_size = a2.size_pages;

        let state = pool.save().unwrap();

        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        pool.restore(state).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let restored_a1 = alloc
            .restore_alloc(a1_pfn, a1_size.try_into().unwrap())
            .unwrap();
        let restored_a2 = alloc
            .restore_alloc(a2_pfn, a2_size.try_into().unwrap())
            .unwrap();

        assert_eq!(restored_a1.base_pfn(), a1_pfn);
        assert_eq!(restored_a1.size_pages, a1_size);

        assert_eq!(restored_a2.base_pfn(), a2_pfn);
        assert_eq!(restored_a2.size_pages, a2_size);

        pool.validate_restore(false).unwrap();
    }

    #[test]
    fn test_save_restore_all_pending() {
        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        let a1_pfn = a1.base_pfn();
        let a1_size = a1.size_pages;

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        let a2_pfn = a2.base_pfn();
        let a2_size = a2.size_pages;

        let state = pool.save().unwrap();

        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        pool.restore(state).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let restored = alloc.restore_pending_allocs();
        assert!(
            restored
                .iter()
                .any(|a| a.base_pfn == a1_pfn && a.size_pages == a1_size),
            "matching allocation not found for a1"
        );

        assert!(
            restored
                .iter()
                .any(|a| a.base_pfn == a2_pfn && a.size_pages == a2_size),
            "matching allocation not found for a2"
        );

        pool.validate_restore(false).unwrap();
    }

    #[test]
    fn test_save_restore_all_pending_other_allocator() {
        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        let a1_pfn = a1.base_pfn();
        let a1_size = a1.size_pages;

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        let a2_pfn = a2.base_pfn();
        let a2_size = a2.size_pages;

        let state = pool.save().unwrap();

        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();
        pool.restore(state).unwrap();
        let alloc = pool.allocator("test2".into()).unwrap();

        let restored = alloc.restore_pending_allocs();
        assert_eq!(
            restored
                .iter()
                .any(|a| a.base_pfn == a1_pfn && a.size_pages == a1_size),
            false,
            "matching allocation found for a1 for wrong allocator"
        );

        assert_eq!(
            restored
                .iter()
                .any(|a| a.base_pfn == a2_pfn && a.size_pages == a2_size),
            false,
            "matching allocation found for a2 for wrong allocator"
        );

        assert!(pool.validate_restore(false).is_err());
    }

    #[test]
    fn test_save_restore_unmatched_allocations() {
        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();

        let alloc = pool.allocator("test".into()).unwrap();
        let _a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();

        let state = pool.save().unwrap();

        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();

        pool.restore(state).unwrap();

        assert!(pool.validate_restore(false).is_err());
    }

    #[test]
    fn test_restore_other_allocator() {
        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();

        let alloc = pool.allocator("test".into()).unwrap();
        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();

        let state = pool.save().unwrap();

        let mut pool =
            PagePool::new(&[MemoryRange::from_4k_gpn_range(10..30)], big_test_mapper()).unwrap();

        pool.restore(state).unwrap();

        let alloc = pool.allocator("test2".into()).unwrap();
        assert!(
            alloc
                .restore_alloc(a1.base_pfn, a1.size_pages.try_into().unwrap())
                .is_err()
        );
    }

    #[test]
    fn test_mapping() {
        let pool = PagePool::new(
            &[MemoryRange::from_4k_gpn_range(0..30)],
            TestMapper::new(30).unwrap(),
        )
        .unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        let a1_mapping = a1.mapping();
        assert_eq!(a1_mapping.len(), 5 * PAGE_SIZE as usize);
        a1_mapping[123..][..4].atomic_write(&[1, 2, 3, 4]);
        let mut data = [0; 4];
        a1_mapping[123..][..4].atomic_read(&mut data);
        assert_eq!(data, [1, 2, 3, 4]);
        let mut data = [0; 2];
        a1_mapping[125..][..2].atomic_read(&mut data);
        assert_eq!(data, [3, 4]);
    }
}
