// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

//! This module implements a fixed memory allocator for allocating pages at specific location.

#![cfg(unix)]
#![warn(missing_docs)]

mod mapped_dma;

pub use mapped_dma::FixedDmaBuffer;

use anyhow::Context;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use memory_range::MemoryRange;
use parking_lot::Mutex;
use std::num::NonZeroU64;
use std::sync::Arc;
use thiserror::Error;
use user_driver::memory::MemoryBlock;
use user_driver::vfio::VfioDmaBuffer;

/// Error returned when unable to allocate memory.
#[derive(Debug, Error)]
#[error("unable to allocate fixed pool size {size} with tag {tag}")]
pub struct FixedPoolOutOfMemory {
    size: u64,
    tag: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Inspect)]
#[inspect(external_tag)]
enum State {
    Free {
        #[inspect(hex)]
        base_pfn: u64,
        #[inspect(hex)]
        size_pages: u64,
    },
    Allocated {
        #[inspect(hex)]
        base_pfn: u64,
        #[inspect(hex)]
        size_pages: u64,
        tag: String,
    },
}

#[derive(Inspect, Debug)]
struct FixedPoolInner {
    #[inspect(iter_by_index)]
    state: Vec<State>,
}

/// A handle for fixed pool allocation. When dropped, the allocation is freed.
#[derive(Debug)]
pub struct FixedPoolHandle {
    inner: Arc<Mutex<FixedPoolInner>>,
    base_pfn: u64,
    size_pages: u64,
}

impl FixedPoolHandle {
    /// The base pfn for this allocation.
    pub fn base_pfn(&self) -> u64 {
        self.base_pfn
    }

    /// The number of 4K pages for this allocation.
    pub fn size_pages(&self) -> u64 {
        self.size_pages
    }
}

impl Drop for FixedPoolHandle {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();

        let index = inner
            .state
            .iter()
            .position(|state| {
                if let State::Allocated {
                    base_pfn: base,
                    size_pages: len,
                    tag: _,
                } = state
                {
                    *base == self.base_pfn && *len == self.size_pages
                } else {
                    false
                }
            })
            .expect("must find allocation");

        inner.state[index] = State::Free {
            base_pfn: self.base_pfn,
            size_pages: self.size_pages,
        };
    }
}

/// A page allocator for fixed memory buffer.
///
/// Pages are allocated via [`FixedPoolAllocator`] from [`Self::allocator`].
///
/// This struct is considered the "owner" of the pool allowing for save/restore.
///
#[derive(Inspect)]
pub struct FixedPool {
    #[inspect(flatten)]
    inner: Arc<Mutex<FixedPoolInner>>,
}

impl FixedPool {
    /// Create a fixed pool allocator, with the specified memory.
    pub fn new(fixed_pool: &[MemoryRange]) -> anyhow::Result<Self> {
        let mut pages = Vec::new();
        for range in fixed_pool {
            pages.push(State::Free {
                base_pfn: range.start() / HV_PAGE_SIZE,
                size_pages: range.len() / HV_PAGE_SIZE,
            });
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(FixedPoolInner { state: pages })),
        })
    }

    /// Return an allocator instance that can be used to allocate pages.
    pub fn allocator(&self) -> FixedPoolAllocator {
        FixedPoolAllocator {
            inner: self.inner.clone(),
        }
    }
}

/// A page allocator for fixed memory.
///
/// Pages are allocated via the [`Self::alloc`] method and freed by dropping the
/// associated handle returned.
#[derive(Clone, Debug)]
pub struct FixedPoolAllocator {
    inner: Arc<Mutex<FixedPoolInner>>,
}

impl FixedPoolAllocator {
    /// Allocate contiguous pages from the fixed pool with the given
    /// tag. If a contiguous region of free pages is not available, then an
    /// error is returned.
    pub fn alloc(
        &self,
        size_pages: NonZeroU64,
        tag: String,
    ) -> Result<FixedPoolHandle, FixedPoolOutOfMemory> {
        let mut inner = self.inner.lock();
        let size_pages = size_pages.get();

        let index = inner
            .state
            .iter()
            .position(|state| match state {
                State::Free {
                    base_pfn: _,
                    size_pages: len,
                } => *len >= size_pages,
                State::Allocated { .. } => false,
            })
            .ok_or(FixedPoolOutOfMemory {
                size: size_pages,
                tag: tag.clone(),
            })?;

        let base_pfn = match inner.state.swap_remove(index) {
            State::Free {
                base_pfn: base,
                size_pages: len,
            } => {
                inner.state.push(State::Allocated {
                    base_pfn: base,
                    size_pages,
                    tag,
                });

                if len > size_pages {
                    inner.state.push(State::Free {
                        base_pfn: base + size_pages,
                        size_pages: len - size_pages,
                    });
                }

                base
            }
            State::Allocated { .. } => unreachable!(),
        };

        Ok(FixedPoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            size_pages,
        })
    }
}

impl VfioDmaBuffer for FixedPoolAllocator {
    /// Create new DMA buffer in heap memory.
    fn create_dma_buffer(&self, len: usize) -> anyhow::Result<MemoryBlock> {
        if len == 0 {
            anyhow::bail!("allocation of size 0 not supported");
        }

        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = len as u64 / HV_PAGE_SIZE;
        let alloc = self
            .alloc(
                size_pages.try_into().expect("already checked nonzero"),
                "mshv dma".into(),
            )
            .context("failed to allocate fixed mem")?;

        let gpa_fd = hcl::ioctl::MshvVtlLow::new().context("failed to open gpa fd")?;
        let mapping = sparse_mmap::SparseMapping::new(len).context("failed to create mapping")?;
        let gpa = alloc.base_pfn() * HV_PAGE_SIZE;

        // No need to set bit 63 because this buffer is visible to VTL2 only.
        mapping
            .map_file(0, len, gpa_fd.get(), gpa, true)
            .context("sparse mapping failed")?;
        // Zeroinit the memory, there are servicing cases where this is needed.
        mapping.fill_at(0, 0, len)?;

        let pfns: Vec<_> = (alloc.base_pfn()..alloc.base_pfn() + alloc.size_pages).collect();

        Ok(MemoryBlock::new(FixedDmaBuffer {
            mapping,
            _alloc: alloc,
            pfns,
        }))
    }

    /// Restore DMA buffer at the same location after servicing.
    fn restore_dma_buffer(&self, len: usize, pfns: &[u64]) -> anyhow::Result<MemoryBlock> {
        if len == 0 {
            anyhow::bail!("allocation of size 0 not supported");
        }

        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = len as u64 / HV_PAGE_SIZE;
        assert_eq!(size_pages as usize, pfns.len());

        let alloc = self
            .alloc(
                size_pages.try_into().expect("already checked nonzero"),
                "mshv dma".into(),
            )
            .context("failed to allocate fixed mem")?;

        let gpa_fd = hcl::ioctl::MshvVtlLow::new().context("failed to open gpa fd")?;
        let mapping = sparse_mmap::SparseMapping::new(len).context("failed to create mapping")?;
        let gpa = alloc.base_pfn() * HV_PAGE_SIZE;

        // No need to set bit 63 because this buffer is visible to VTL2 only.
        mapping
            .map_file(0, len, gpa_fd.get(), gpa, true)
            .context("unable to map allocation")?;

        let pfns: Vec<_> = (alloc.base_pfn()..alloc.base_pfn() + alloc.size_pages).collect();

        Ok(MemoryBlock::new(FixedDmaBuffer {
            mapping,
            _alloc: alloc,
            pfns,
        }))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_fixed_alloc() {
        let state = vec![State::Free {
            base_pfn: 10,
            size_pages: 20,
        }];
        let alloc = FixedPoolAllocator {
            inner: Arc::new(Mutex::new(FixedPoolInner { state })),
        };

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        assert_eq!(a1.base_pfn, 10);
        assert_eq!(a1.size_pages, 5);

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        assert_eq!(a2.base_pfn, 15);
        assert_eq!(a2.size_pages, 15);

        assert!(alloc.alloc(1.try_into().unwrap(), "failed".into()).is_err());

        drop(a1);
        drop(a2);

        let inner = alloc.inner.lock();
        assert_eq!(inner.state.len(), 2);
    }
}
