// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements a page memory allocator for allocating pages from a
//! given portion of the guest address space.

#![warn(missing_docs)]

mod device_dma;

pub use device_dma::PagePoolDmaBuffer;

use anyhow::Context;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use inspect::Response;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use sparse_mmap::alloc_shared_memory;
use sparse_mmap::Mappable;
use sparse_mmap::SparseMapping;
use std::fmt::Debug;
use std::num::NonZeroU64;
use std::sync::Arc;
use thiserror::Error;
use vm_topology::memory::MemoryRangeWithNode;

/// Save restore suport for [`PagePool`].
pub mod save_restore {
    use super::PagePool;
    use super::Slot;
    use super::SlotState;
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
            let state = self.inner.lock();
            Ok(PagePoolState {
                state: state
                    .slots
                    .iter()
                    .map(|slot| {
                        let inner_state = match &slot.state {
                            SlotState::Free => InnerSlotState::Free,
                            SlotState::Allocated { device_id, tag } => InnerSlotState::Allocated {
                                device_id: state.device_ids[*device_id].name().to_string(),
                                tag: tag.clone(),
                            },
                            SlotState::Leaked { device_id, tag } => InnerSlotState::Leaked {
                                device_id: device_id.clone(),
                                tag: tag.clone(),
                            },
                            SlotState::AllocatedPendingRestore { .. } => {
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
            state: Self::SavedState,
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

            let mut inner = self.inner.lock();

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

                    Slot {
                        base_pfn: slot.base_pfn,
                        size_pages: slot.size_pages,
                        state: inner,
                    }
                })
                .collect();

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

    fn name(&self) -> &str {
        match self {
            SlotState::Free => "free",
            SlotState::Allocated { .. } => "allocated",
            SlotState::AllocatedPendingRestore { .. } => "allocated_pending_restore",
            SlotState::Leaked { .. } => "leaked",
        }
    }
}

/// What kind of memory this pool is.
#[derive(Inspect, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolType {
    /// Private memory, that is not visible to the host.
    Private,
    /// Shared memory, that is visible to the host. This requires mapping pages
    /// with the decrypted bit set on mmap calls.
    Shared,
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

struct PagePoolInner {
    /// The internal slots for the pool, representing page state.
    slots: Vec<Slot>,
    /// The pfn_bias for the pool.
    pfn_bias: u64,
    /// The list of device ids for outstanding allocators. Each name must be
    /// unique.
    device_ids: Vec<DeviceId>,
    /// The mapper used to create mappings for allocations.
    mapper: Box<dyn Mapper>,
}

// Manually implement inspect so device_ids can be rendered as strings, not
// their actual usize index.
impl Inspect for PagePoolInner {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("device_ids", inspect::iter_by_index(&self.device_ids))
            .field("mapper", &self.mapper)
            .child("slots", |req| {
                let mut resp = req.respond();
                for (i, slot) in self.slots.iter().enumerate() {
                    resp.child(&i.to_string(), |req| {
                        let mut resp = req.respond();
                        resp.field("base_pfn", inspect::AsHex(slot.base_pfn))
                            .field("size_pages", inspect::AsHex(slot.size_pages))
                            .field("state", slot.state.name());

                        match &slot.state {
                            SlotState::Free => {}
                            SlotState::Allocated { device_id, tag } => {
                                resp.field("device_id", self.device_ids[*device_id].name())
                                    .field("tag", tag);
                            }
                            SlotState::AllocatedPendingRestore { device_id, tag }
                            | SlotState::Leaked { device_id, tag } => {
                                resp.field("device_id", device_id.clone()).field("tag", tag);
                            }
                        }
                    });
                }
            });
    }
}

/// A handle for a page pool allocation. When dropped, the allocation is
/// freed.
pub struct PagePoolHandle {
    inner: Arc<Mutex<PagePoolInner>>,
    base_pfn: u64,
    pfn_bias: u64,
    size_pages: u64,
    mapping: Option<SparseMapping>,
}

impl PagePoolHandle {
    /// The base pfn (with bias) for this allocation.
    pub fn base_pfn(&self) -> u64 {
        self.base_pfn + self.pfn_bias
    }

    /// The base pfn without bias for this allocation.
    pub fn base_pfn_without_bias(&self) -> u64 {
        self.base_pfn
    }

    /// The number of 4K pages for this allocation.
    pub fn size_pages(&self) -> u64 {
        self.size_pages
    }

    /// The associated mapping with this allocation. This is only available if
    /// this was allocated with [`PagePoolAllocator::alloc_with_mapping`].
    pub fn mapping(&self) -> Option<&SparseMapping> {
        self.mapping.as_ref()
    }

    /// Create a memory block from this allocation.
    fn into_memory_block(
        mut self,
        zero_block: bool,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        // Take ownership of the mapping, as the outer memory block type will
        // guarantee the mapping is dropped when the allocation is also dropped.
        let mapping = self
            .mapping
            .take()
            .context("allocation did not have associated mapping")?;

        // Zero memory block if requested.
        if zero_block {
            let len = (self.size_pages * HV_PAGE_SIZE) as usize;
            mapping
                .fill_at(0, 0, len)
                .context("failed to zero allocated memory")?;
        }

        let pfns: Vec<_> = (self.base_pfn()..self.base_pfn() + self.size_pages).collect();
        let pfn_bias = self.pfn_bias;

        Ok(user_driver::memory::MemoryBlock::new(PagePoolDmaBuffer {
            mapping,
            _alloc: self,
            pfns,
            pfn_bias,
        }))
    }
}

impl Drop for PagePoolHandle {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();

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

        slot.state = SlotState::Free;
    }
}

/// A trait used to map a range of pages into a [`SparseMapping`].
pub trait Mapper: Inspect + Send + Sync {
    /// Create a mapping for the given range of pages.
    ///
    /// The pages should be mapped such that the `base_pfn` is at offset zero,
    /// with the `size_pages` being the total size of the mapping.
    fn map(
        &self,
        base_pfn: u64,
        size_pages: u64,
        pool_type: PoolType,
    ) -> Result<SparseMapping, anyhow::Error>;
}

/// A mapper that does not support mapping and always returns an error.
#[derive(Inspect)]
#[inspect(extra = "NoMapper::inspect_extra")]
pub struct NoMapper;

impl NoMapper {
    fn inspect_extra(&self, resp: &mut Response<'_>) {
        resp.field("type", "unsupported");
    }
}

impl Mapper for NoMapper {
    fn map(
        &self,
        _base_pfn: u64,
        _size_pages: u64,
        _pool_type: PoolType,
    ) -> Result<SparseMapping, anyhow::Error> {
        anyhow::bail!("mapping not supported on this pool")
    }
}

/// A mapper that uses an internal buffer to map pages. This is meant to be used
/// for tests that use [`PagePool`].
#[derive(Inspect)]
#[inspect(extra = "TestMapper::inspect_extra")]
pub struct TestMapper {
    #[inspect(skip)]
    mem: Mappable,
}

impl TestMapper {
    /// Create a new test mapper that holds an internal buffer of `size_pages`.
    pub fn new(size_pages: u64) -> anyhow::Result<Self> {
        let len = (size_pages * HV_PAGE_SIZE) as usize;
        let fd = alloc_shared_memory(len).context("creating shared mem")?;

        Ok(Self { mem: fd })
    }

    fn inspect_extra(&self, resp: &mut Response<'_>) {
        resp.field("type", "test");
    }
}

impl Mapper for TestMapper {
    fn map(
        &self,
        base_pfn: u64,
        size_pages: u64,
        _pool_type: PoolType,
    ) -> Result<SparseMapping, anyhow::Error> {
        let len = (size_pages * HV_PAGE_SIZE) as usize;
        let mapping = SparseMapping::new(len).context("failed to create mapping")?;
        let gpa = base_pfn * HV_PAGE_SIZE;

        mapping
            .map_file(0, len, &self.mem, gpa, true)
            .context("unable to map allocation")?;

        Ok(mapping)
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
    inner: Arc<Mutex<PagePoolInner>>,
    #[inspect(iter_by_index)]
    ranges: Vec<MemoryRange>,
    typ: PoolType,
}

impl PagePool {
    /// Create a new private pool allocator, with the specified memory. The
    /// memory must not be used by any other entity.
    pub fn new_private_pool<T: Mapper + 'static>(
        private_pool: &[MemoryRangeWithNode],
        mapper: T,
    ) -> anyhow::Result<Self> {
        Self::new_internal(private_pool, PoolType::Private, 0, mapper)
    }

    /// Create a shared visibility page pool allocator, with the specified
    /// memory. The supplied guest physical address ranges must be in the
    /// correct shared state and usable. The memory must not be used by any
    /// other entity.
    ///
    /// `addr_bias` represents a bias to apply to addresses in `shared_pool`.
    /// This should be vtom on hardware isolated platforms.
    pub fn new_shared_visibility_pool<T: Mapper + 'static>(
        shared_pool: &[MemoryRangeWithNode],
        addr_bias: u64,
        mapper: T,
    ) -> anyhow::Result<Self> {
        Self::new_internal(shared_pool, PoolType::Shared, addr_bias, mapper)
    }

    fn new_internal<T: Mapper + 'static>(
        memory: &[MemoryRangeWithNode],
        typ: PoolType,
        addr_bias: u64,
        mapper: T,
    ) -> anyhow::Result<Self> {
        // TODO: Allow callers to specify the vnode, but today we discard this
        // information. In the future we may keep ranges with vnode in order to
        // allow per-node allocations.

        let pages = memory
            .iter()
            .map(|range| Slot {
                base_pfn: range.range.start() / HV_PAGE_SIZE,
                size_pages: range.range.len() / HV_PAGE_SIZE,
                state: SlotState::Free,
            })
            .collect();

        Ok(Self {
            inner: Arc::new(Mutex::new(PagePoolInner {
                slots: pages,
                pfn_bias: addr_bias / HV_PAGE_SIZE,
                device_ids: Vec::new(),
                mapper: Box::new(mapper),
            })),
            ranges: memory.iter().map(|r| r.range).collect(),
            typ,
        })
    }

    /// Create an allocator instance that can be used to allocate pages. The
    /// specified `device_name` must be unique.
    ///
    /// Users should create a new allocator for each device, as the device name
    /// is used to track allocations in the pool.
    pub fn allocator(&self, device_name: String) -> anyhow::Result<PagePoolAllocator> {
        PagePoolAllocator::new(&self.inner, self.typ, device_name)
    }

    /// Create a spawner that allows creating multiple allocators.
    pub fn allocator_spawner(&self) -> PagePoolAllocatorSpawner {
        PagePoolAllocatorSpawner {
            inner: self.inner.clone(),
            typ: self.typ,
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
        let mut inner = self.inner.lock();
        let mut unrestored_allocation = false;

        // Mark unrestored allocations as leaked.
        for slot in inner.slots.iter_mut() {
            match &slot.state {
                SlotState::Free | SlotState::Allocated { .. } | SlotState::Leaked { .. } => {}
                SlotState::AllocatedPendingRestore { device_id, tag } => {
                    tracing::warn!(
                        base_pfn = slot.base_pfn,
                        pfn_bias = slot.size_pages,
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
    inner: Arc<Mutex<PagePoolInner>>,
    typ: PoolType,
}

impl PagePoolAllocatorSpawner {
    /// Create an allocator instance that can be used to allocate pages. The
    /// specified `device_name` must be unique.
    ///
    /// Users should create a new allocator for each device, as the device name
    /// is used to track allocations in the pool.
    pub fn allocator(&self, device_name: String) -> anyhow::Result<PagePoolAllocator> {
        PagePoolAllocator::new(&self.inner, self.typ, device_name)
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
pub struct PagePoolAllocator {
    inner: Arc<Mutex<PagePoolInner>>,
    typ: PoolType,
    device_id: usize,
}

impl PagePoolAllocator {
    fn new(
        inner: &Arc<Mutex<PagePoolInner>>,
        typ: PoolType,
        device_name: String,
    ) -> anyhow::Result<Self> {
        let device_id;
        {
            let mut inner = inner.lock();

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
            typ,
            device_id,
        })
    }

    fn alloc_inner(
        &self,
        size_pages: NonZeroU64,
        tag: String,
        with_mapping: bool,
    ) -> Result<PagePoolHandle, Error> {
        let mut inner = self.inner.lock();
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

        let pfn_bias = inner.pfn_bias;

        // Track which slots we should append if the mapping creation succeeds.
        // If the mapping creation fails, we instead commit the original free
        // slot back to the pool.
        let (original_slot, allocation_slot, free_slot) = {
            let slot = inner.slots.swap_remove(index);
            assert!(matches!(slot.state, SlotState::Free));

            let allocation_slot = Slot {
                base_pfn: slot.base_pfn,
                size_pages,
                state: SlotState::Allocated {
                    device_id: self.device_id,
                    tag: tag.clone(),
                },
            };

            let free_slot = if slot.size_pages > size_pages {
                Some(Slot {
                    base_pfn: slot.base_pfn + size_pages,
                    size_pages: slot.size_pages - size_pages,
                    state: SlotState::Free,
                })
            } else {
                None
            };

            (slot, allocation_slot, free_slot)
        };

        let base_pfn = allocation_slot.base_pfn;

        let mapping = if with_mapping {
            let mapping = match inner.mapper.map(base_pfn, size_pages, self.typ) {
                Ok(mapping) => mapping,
                Err(e) => {
                    // Commit the original slot back to the pool.
                    inner.slots.push(original_slot);

                    return Err(Error::Mapping(e));
                }
            };

            Some(mapping)
        } else {
            None
        };

        // Commit state to the pool.
        inner.slots.push(allocation_slot);
        if let Some(free_slot) = free_slot {
            inner.slots.push(free_slot);
        }

        Ok(PagePoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            pfn_bias,
            size_pages,
            mapping,
        })
    }

    /// Allocate contiguous pages from the page pool with the given tag. If a
    /// contiguous region of free pages is not available, then an error is
    /// returned.
    pub fn alloc(&self, size_pages: NonZeroU64, tag: String) -> Result<PagePoolHandle, Error> {
        self.alloc_inner(size_pages, tag, false)
    }

    /// The same as [`Self::alloc`], but also creates an associated mapping for
    /// the allocation so the user can use the mapping via
    /// [`PagePoolHandle::mapping`].
    pub fn alloc_with_mapping(
        &self,
        size_pages: NonZeroU64,
        tag: String,
    ) -> Result<PagePoolHandle, Error> {
        self.alloc_inner(size_pages, tag, true)
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
        with_mapping: bool,
    ) -> Result<PagePoolHandle, Error> {
        let size_pages = size_pages.get();
        let mut inner = self.inner.lock();
        let index = inner
            .slots
            .iter()
            .position(|slot| {
                if let SlotState::AllocatedPendingRestore { device_id, tag: _ } = &slot.state {
                    device_id == inner.device_ids[self.device_id].name()
                        && slot.base_pfn == base_pfn
                        && slot.size_pages == size_pages
                } else {
                    false
                }
            })
            .ok_or(Error::NoMatchingAllocation)?;

        let mapping = if with_mapping {
            let mapping = inner
                .mapper
                .map(base_pfn, size_pages, self.typ)
                .map_err(Error::Mapping)?;
            Some(mapping)
        } else {
            None
        };

        inner.slots[index].state.restore_allocated(self.device_id);

        Ok(PagePoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            pfn_bias: inner.pfn_bias,
            size_pages,
            mapping,
        })
    }
}

impl Drop for PagePoolAllocator {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
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
        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = NonZeroU64::new(len as u64 / HV_PAGE_SIZE)
            .context("allocation of size 0 not supported")?;

        let alloc = self
            .alloc_with_mapping(size_pages, "vfio dma".into())
            .context("failed to allocate shared mem")?;

        // The VfioDmaBuffer trait requires that newly allocated buffers are
        // zeroed.
        alloc.into_memory_block(true)
    }

    /// Restore a dma buffer in the predefined location with the given `len` in
    /// bytes.
    fn attach_dma_buffer(
        &self,
        len: usize,
        base_pfn: u64,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = NonZeroU64::new(len as u64 / HV_PAGE_SIZE)
            .context("allocation of size 0 not supported")?;

        let alloc = self
            .restore_alloc(base_pfn, size_pages, true)
            .context("failed to restore allocation")?;

        // Preserve the existing contents of memory and do not zero the restored
        // allocation.
        alloc.into_memory_block(false)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use memory_range::MemoryRange;
    use vmcore::save_restore::SaveRestore;

    #[test]
    fn test_basic_alloc() {
        let pfn_bias = 15;
        let pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            pfn_bias * HV_PAGE_SIZE,
            NoMapper,
        )
        .unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        assert_eq!(a1.base_pfn, 10);
        assert_eq!(a1.pfn_bias, pfn_bias);
        assert_eq!(a1.base_pfn(), a1.base_pfn + a1.pfn_bias);
        assert_eq!(a1.base_pfn_without_bias(), a1.base_pfn);
        assert_eq!(a1.size_pages, 5);

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        assert_eq!(a2.base_pfn, 15);
        assert_eq!(a2.pfn_bias, pfn_bias);
        assert_eq!(a2.base_pfn(), a2.base_pfn + a2.pfn_bias);
        assert_eq!(a2.base_pfn_without_bias(), a2.base_pfn);
        assert_eq!(a2.size_pages, 15);

        assert!(alloc.alloc(1.try_into().unwrap(), "failed".into()).is_err());

        drop(a1);
        drop(a2);

        let inner = alloc.inner.lock();
        assert_eq!(inner.slots.len(), 2);
    }

    #[test]
    fn test_duplicate_device_name() {
        let pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();
        let _alloc = pool.allocator("test".into()).unwrap();

        assert!(pool.allocator("test".into()).is_err());
    }

    #[test]
    fn test_dropping_allocator() {
        let pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..40),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();
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
        let mut pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        let a1_pfn = a1.base_pfn();
        let a1_pfn_bias = a1.pfn_bias;
        let a1_size = a1.size_pages;

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        let a2_pfn = a2.base_pfn();
        let a2_pfn_bias = a2.pfn_bias;
        let a2_size = a2.size_pages;

        let state = pool.save().unwrap();

        let mut pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();
        pool.restore(state).unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let restored_a1 = alloc
            .restore_alloc(a1_pfn, a1_size.try_into().unwrap(), false)
            .unwrap();
        let restored_a2 = alloc
            .restore_alloc(a2_pfn, a2_size.try_into().unwrap(), false)
            .unwrap();

        assert_eq!(restored_a1.base_pfn(), a1_pfn);
        assert_eq!(restored_a1.pfn_bias, a1_pfn_bias);
        assert_eq!(restored_a1.size_pages, a1_size);

        assert_eq!(restored_a2.base_pfn(), a2_pfn);
        assert_eq!(restored_a2.pfn_bias, a2_pfn_bias);
        assert_eq!(restored_a2.size_pages, a2_size);

        pool.validate_restore(false).unwrap();
    }

    #[test]
    fn test_save_restore_unmatched_allocations() {
        let mut pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();

        let alloc = pool.allocator("test".into()).unwrap();
        let _a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();

        let state = pool.save().unwrap();

        let mut pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();

        pool.restore(state).unwrap();

        assert!(pool.validate_restore(false).is_err());
    }

    #[test]
    fn test_restore_other_allocator() {
        let mut pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();

        let alloc = pool.allocator("test".into()).unwrap();
        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();

        let state = pool.save().unwrap();

        let mut pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
            NoMapper,
        )
        .unwrap();

        pool.restore(state).unwrap();

        let alloc = pool.allocator("test2".into()).unwrap();
        assert!(alloc
            .restore_alloc(a1.base_pfn, a1.size_pages.try_into().unwrap(), false)
            .is_err());
    }

    #[test]
    fn test_mapping() {
        let pool = PagePool::new_private_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(0..30),
                vnode: 0,
            }],
            TestMapper::new(30).unwrap(),
        )
        .unwrap();
        let alloc = pool.allocator("test".into()).unwrap();

        let a1 = alloc
            .alloc_with_mapping(5.try_into().unwrap(), "alloc1".into())
            .unwrap();
        let a1_mapping = a1.mapping().unwrap();
        assert_eq!(a1_mapping.len(), 5 * HV_PAGE_SIZE as usize);
        a1_mapping.write_at(123, &[1, 2, 3, 4]).unwrap();
        let mut data = [0; 4];
        a1_mapping.read_at(123, &mut data).unwrap();
        assert_eq!(data, [1, 2, 3, 4]);
        let mut data = [0; 2];
        a1_mapping.read_at(125, &mut data).unwrap();
        assert_eq!(data, [3, 4]);
    }
}
