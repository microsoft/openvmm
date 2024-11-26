// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements a page memory allocator for allocating pages from a
//! given portion of the guest address space.

#![cfg(unix)]
#![warn(missing_docs)]

mod device_dma;

pub use device_dma::PagePoolDmaBuffer;

#[cfg(all(feature = "vfio", target_os = "linux"))]
use anyhow::Context;
#[cfg(all(feature = "vfio", target_os = "linux"))]
use hcl::ioctl::MshvVtlLow;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use parking_lot::Mutex;
use std::num::NonZeroU64;
use std::sync::Arc;
use thiserror::Error;
use vm_topology::memory::MemoryRangeWithNode;

/// Error returned when unable to allocate memory.
#[derive(Debug, Error)]
#[error("unable to allocate page pool size {size} with tag {tag}")]
pub struct PagePoolOutOfMemory {
    size: u64,
    tag: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Free {
        base_pfn: u64,
        pfn_bias: u64,
        size_pages: u64,
    },
    Allocated {
        base_pfn: u64,
        pfn_bias: u64,
        size_pages: u64,
        /// This is an index into the outer [`PagePoolInner`]'s device_ids
        /// vector.
        device_id: usize,
        tag: String,
    },
}

#[derive(Inspect, Debug, Clone, Copy, PartialEq, Eq)]
enum PoolType {
    // Private memory, that is not visible to the host.
    Private,
    // Shared memory, that is visible to the host. This requires mapping pages
    // with the decrypted bit set on mmap calls.
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

#[derive(Debug)]
struct PagePoolInner {
    /// The internal state of the pool.
    state: Vec<State>,
    /// The list of device ids for outstanding allocators. Each name must be
    /// unique.
    device_ids: Vec<DeviceId>,
}

// Manually implement inspect so device_ids can be rendered as strings, not
// their actual usize index.
impl Inspect for PagePoolInner {
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond()
            .field("device_ids", inspect::iter_by_index(&self.device_ids))
            .child("state", |req| {
                let mut resp = req.respond();
                for (i, state) in self.state.iter().enumerate() {
                    resp.child(&i.to_string(), |req| match state {
                        State::Free {
                            base_pfn,
                            pfn_bias,
                            size_pages,
                        } => {
                            req.respond()
                                .field("state", "free")
                                .field("base_pfn", inspect::AsHex(base_pfn))
                                .field("pfn_bias", inspect::AsHex(pfn_bias))
                                .field("size_pages", inspect::AsHex(size_pages));
                        }
                        State::Allocated {
                            base_pfn,
                            pfn_bias,
                            size_pages,
                            device_id,
                            tag,
                        } => {
                            req.respond()
                                .field("state", "allocated")
                                .field("base_pfn", inspect::AsHex(base_pfn))
                                .field("pfn_bias", inspect::AsHex(pfn_bias))
                                .field("size_pages", inspect::AsHex(size_pages))
                                .field("device_id", self.device_ids[*device_id].clone())
                                .field("tag", tag);
                        }
                    });
                }
            });
    }
}

/// A handle for a page pool allocation. When dropped, the allocation is
/// freed.
#[derive(Debug)]
pub struct PagePoolHandle {
    inner: Arc<Mutex<PagePoolInner>>,
    base_pfn: u64,
    pfn_bias: u64,
    size_pages: u64,
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
}

impl Drop for PagePoolHandle {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();

        let entry = inner
            .state
            .iter_mut()
            .find(|state| {
                if let State::Allocated {
                    base_pfn,
                    pfn_bias,
                    size_pages,
                    device_id: _,
                    tag: _,
                } = state
                {
                    *base_pfn == self.base_pfn
                        && *pfn_bias == self.pfn_bias
                        && *size_pages == self.size_pages
                } else {
                    false
                }
            })
            .expect("must find allocation");

        *entry = State::Free {
            base_pfn: self.base_pfn,
            pfn_bias: self.pfn_bias,
            size_pages: self.size_pages,
        };
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
///
// TODO SNP: Implement save restore. This means additionally having some sort of
// restore_alloc method that maps to an existing allocation.
#[derive(Inspect)]
pub struct PagePool {
    #[inspect(flatten)]
    inner: Arc<Mutex<PagePoolInner>>,
    typ: PoolType,
}

impl PagePool {
    /// Create a new private pool allocator, with the specified memory. The
    /// memory must not be used by any other entity.
    pub fn new_private_pool(private_pool: &[MemoryRangeWithNode]) -> anyhow::Result<Self> {
        Self::new_internal(private_pool, PoolType::Private, 0)
    }

    /// Create a shared visibility page pool allocator, with the specified
    /// memory. The supplied guest physical address ranges must be in the
    /// correct shared state and usable. The memory must not be used by any
    /// other entity.
    ///
    /// `addr_bias` represents a bias to apply to addresses in `shared_pool`.
    /// This should be vtom on hardware isolated platforms.
    pub fn new_shared_visibility_pool(
        shared_pool: &[MemoryRangeWithNode],
        addr_bias: u64,
    ) -> anyhow::Result<Self> {
        Self::new_internal(shared_pool, PoolType::Shared, addr_bias)
    }

    fn new_internal(
        memory: &[MemoryRangeWithNode],
        typ: PoolType,
        addr_bias: u64,
    ) -> anyhow::Result<Self> {
        let pages = memory
            .iter()
            .map(|range| State::Free {
                base_pfn: range.range.start() / HV_PAGE_SIZE,
                pfn_bias: addr_bias / HV_PAGE_SIZE,
                size_pages: range.range.len() / HV_PAGE_SIZE,
            })
            .collect();

        Ok(Self {
            inner: Arc::new(Mutex::new(PagePoolInner {
                state: pages,
                device_ids: Vec::new(),
            })),
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

    // TODO: save method and restore
}

/// A spawner for [`PagePoolAllocator`] instances.
///
/// Useful when you need to create multiple allocators, without having ownership
/// of the actual [`PagePool`].
#[derive(Debug)]
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
#[derive(Debug)]
pub struct PagePoolAllocator {
    inner: Arc<Mutex<PagePoolInner>>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

            // Device ID must be unique, or be unassigned.
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

    /// Allocate contiguous pages from the page pool with the given tag. If a
    /// contiguous region of free pages is not available, then an error is
    /// returned.
    pub fn alloc(
        &self,
        size_pages: NonZeroU64,
        tag: String,
    ) -> Result<PagePoolHandle, PagePoolOutOfMemory> {
        let mut inner = self.inner.lock();
        let size_pages = size_pages.get();

        let index = inner
            .state
            .iter()
            .position(|state| match state {
                State::Free {
                    base_pfn: _,
                    pfn_bias: _,
                    size_pages: len,
                } => *len >= size_pages,
                State::Allocated { .. } => false,
            })
            .ok_or(PagePoolOutOfMemory {
                size: size_pages,
                tag: tag.clone(),
            })?;

        let (base_pfn, pfn_bias) = match inner.state.swap_remove(index) {
            State::Free {
                base_pfn: base,
                pfn_bias: offset,
                size_pages: len,
            } => {
                inner.state.push(State::Allocated {
                    base_pfn: base,
                    pfn_bias: offset,
                    size_pages,
                    device_id: self.device_id,
                    tag,
                });

                if len > size_pages {
                    inner.state.push(State::Free {
                        base_pfn: base + size_pages,
                        pfn_bias: offset,
                        size_pages: len - size_pages,
                    });
                }

                (base, offset)
            }
            State::Allocated { .. } => unreachable!(),
        };

        Ok(PagePoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            pfn_bias,
            size_pages,
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

#[cfg(all(feature = "vfio", target_os = "linux"))]
impl user_driver::vfio::VfioDmaBuffer for PagePoolAllocator {
    fn create_dma_buffer(&self, len: usize) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = NonZeroU64::new(len as u64 / HV_PAGE_SIZE)
            .context("allocation of size 0 not supported")?;

        let alloc = self
            .alloc(size_pages, "vfio dma".into())
            .context("failed to allocate shared mem")?;

        let gpa_fd = MshvVtlLow::new().context("failed to open gpa fd")?;
        let mapping = sparse_mmap::SparseMapping::new(len).context("failed to create mapping")?;

        let gpa = alloc.base_pfn() * HV_PAGE_SIZE;

        // When the pool references shared memory, on hardware isolated
        // platforms the file_offset must have the shared bit set as these are
        // decrypted pages. Setting this bit is okay on non-hardware isolated
        // platforms, as it does nothing.
        let file_offset = match self.typ {
            PoolType::Private => gpa,
            PoolType::Shared => {
                tracing::trace!("setting MshvVtlLow::SHARED_MEMORY_FLAG");
                gpa | MshvVtlLow::SHARED_MEMORY_FLAG
            }
        };

        tracing::trace!(gpa, file_offset, len, "mapping dma buffer");
        mapping
            .map_file(0, len, gpa_fd.get(), file_offset, true)
            .context("unable to map allocation")?;

        // The VfioDmaBuffer trait requires that allocated buffers are zeroed.
        mapping
            .fill_at(0, 0, len)
            .context("failed to zero allocated memory")?;

        let pfns: Vec<_> = (alloc.base_pfn()..alloc.base_pfn() + alloc.size_pages).collect();

        Ok(user_driver::memory::MemoryBlock::new(PagePoolDmaBuffer {
            mapping,
            _alloc: alloc,
            pfns,
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use memory_range::MemoryRange;

    #[test]
    fn test_basic_alloc() {
        let pfn_bias = 15;
        let pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            pfn_bias * HV_PAGE_SIZE,
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
        assert_eq!(inner.state.len(), 2);
    }

    #[test]
    fn test_duplicate_device_name() {
        let pool = PagePool::new_shared_visibility_pool(
            &[MemoryRangeWithNode {
                range: MemoryRange::from_4k_gpn_range(10..30),
                vnode: 0,
            }],
            0,
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
}
