// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Copyright (C) Microsoft Corporation. All rights reserved.

//! This module implements a fixed memory allocator for allocating pages at specific location.

#![cfg(unix)]
#![warn(missing_docs)]
// SAFETY: Send, Sync, and *nix calls mmap() munmap() require unsafe keyword.
#![allow(unsafe_code)]

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
use user_driver::HostDmaAllocator;

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

impl FixedPoolInner {
    /// Add another range to the set of ranges.
    pub(crate) fn add(&mut self, base_pfn: u64, size_pages: u64) {
        self.state.push(State::Free {
            base_pfn,
            size_pages,
        });
    }
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
    pub fn new(fixed_pool: Vec<MemoryRange>) -> anyhow::Result<Self> {
        let mut pages = Vec::new();
        for range in &fixed_pool {
            pages.push(State::Free {
                base_pfn: range.start() / HV_PAGE_SIZE,
                size_pages: range.len() / HV_PAGE_SIZE,
            });
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(FixedPoolInner { state: pages })),
        })
    }

    /// Create an allocator instance that can be used to allocate pages.
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
    /// Reserves fixed memory region for future allocations for DMA devices.
    pub fn new(fixed_pool: Vec<MemoryRange>) -> anyhow::Result<Self> {
        let mut pages = Vec::new();
        for range in &fixed_pool {
            pages.push(State::Free {
                base_pfn: range.start() / HV_PAGE_SIZE,
                size_pages: range.len() / HV_PAGE_SIZE,
            });
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(FixedPoolInner { state: pages })),
        })
    }

    /// Allocate contiguous pages from the fixed pool with the given
    /// tag. If a contiguous region of free pages is not available, then an
    /// error is returned.
    fn alloc(
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

    /// Restore allocation of the contiguous pages from the fixed pool.
    /// If a contiguous region of free pages is not available, then an
    /// error is returned.
    fn restore(
        &self,
        req_pfn: u64,
        req_pages: NonZeroU64,
        tag: String,
    ) -> Result<FixedPoolHandle, FixedPoolOutOfMemory> {
        let mut inner = self.inner.lock();
        let req_pages = req_pages.get();

        let index = inner
            .state
            .iter()
            .position(|state| match state {
                State::Free {
                    base_pfn: avail_pfn,
                    size_pages: avail_pages,
                } => {
                    *avail_pages >= req_pages
                        && *avail_pfn <= req_pfn
                        && *avail_pfn + *avail_pages >= req_pfn + req_pages
                }
                State::Allocated { .. } => false,
            })
            .ok_or(FixedPoolOutOfMemory {
                size: req_pages,
                tag: tag.clone(),
            })?;

        let new_pfn = match inner.state.swap_remove(index) {
            State::Free {
                base_pfn: free_base,
                size_pages: free_len,
            } => {
                // Push the requested block to the collection.
                inner.state.push(State::Allocated {
                    base_pfn: req_pfn,
                    size_pages: req_pages,
                    tag,
                });

                if free_len > req_pages {
                    // Push back the left free range.
                    if free_base < req_pfn {
                        inner.state.push(State::Free {
                            base_pfn: free_base,
                            size_pages: req_pfn - free_base,
                        });
                    }
                    // Push back the right free range.
                    if req_pfn + req_pages < free_base + free_len {
                        inner.state.push(State::Free {
                            base_pfn: req_pfn + req_pages,
                            size_pages: free_base + free_len - req_pfn - req_pages,
                        })
                    }
                }
                req_pfn
            }
            State::Allocated { .. } => unreachable!(),
        };

        Ok(FixedPoolHandle {
            inner: self.inner.clone(),
            base_pfn: new_pfn,
            size_pages: req_pages,
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
        let file_offset = gpa;

        tracing::trace!(gpa, file_offset, len, "mapping dma buffer");
        mapping
            .map_file(0, len, gpa_fd.get(), file_offset, true)
            .context("sparse mapping failed")?;

        let pfns: Vec<_> = (alloc.base_pfn()..alloc.base_pfn() + alloc.size_pages).collect();

        Ok(MemoryBlock::new(FixedDmaBuffer {
            mapping,
            _alloc: alloc,
            pfns,
        }))
    }

    /// Restore DMA buffer at the same location after servicing.
    fn restore_dma_buffer(
        &self,
        len: usize,
        pfns: &[u64],
    ) -> anyhow::Result<MemoryBlock> {
        if len == 0 {
            anyhow::bail!("allocation of size 0 not supported");
        }

        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = len as u64 / HV_PAGE_SIZE;
        assert_eq!(size_pages as usize, pfns.len());

        // This is another hack until we have proper memory mapping.
        let alloc = self
            .alloc(
                size_pages.try_into().expect("already checked nonzero"),
                "mshv dma".into(),
            )
            .context("failed to allocate fixed mem")?;

        let gpa_fd = hcl::ioctl::MshvVtlLow::new().context("failed to open gpa fd")?;
        let mapping = sparse_mmap::SparseMapping::new(len)
            .context("failed to create mapping")?;
        let gpa = alloc.base_pfn() * HV_PAGE_SIZE;

        // No need to set bit 63 because this buffer is visible to VTL2 only.
        let file_offset = gpa;

        tracing::trace!(gpa, file_offset, len, "mapping dma buffer");
        mapping
            .map_file(0, len, gpa_fd.get(), file_offset, true)
            .context("unable to map allocation")?;

        let pfns: Vec<_> = (alloc.base_pfn()..alloc.base_pfn() + alloc.size_pages).collect();

        Ok(MemoryBlock::new(FixedDmaBuffer {
            mapping,
            _alloc: alloc,
            pfns,
        }))
    }
}

impl HostDmaAllocator for FixedPoolAllocator {
    fn allocate_dma_buffer(&self, len: usize) -> anyhow::Result<MemoryBlock> {
        self.create_dma_buffer(len)
    }

    /// Restore contiguous buffer starting with given PFN.
    fn restore_dma_buffer(
        &mut self,
        len: usize,
        pfns: &[u64],
    ) -> anyhow::Result<MemoryBlock> {
        if len == 0 {
            anyhow::bail!("allocation of size 0 not supported");
        }

        if len as u64 % HV_PAGE_SIZE != 0 {
            anyhow::bail!("not a page-size multiple");
        }

        let size_pages = len as u64 / HV_PAGE_SIZE;
        assert_eq!(size_pages as usize, pfns.len());
        assert!(pfns.len() > 0);

        // Allocate from the previously reserved page range.
        let alloc = self
            .alloc(
                size_pages.try_into().expect("already checked nonzero"),
                "mshv dma".into(),
            )
            .context("failed to allocate fixed mem")?;

        let gpa_fd = hcl::ioctl::MshvVtlLow::new().context("failed to open gpa fd")?;
        let mapping =
            sparse_mmap::SparseMapping::new(len).context("failed to create mapping")?;
        let gpa = alloc.base_pfn() * HV_PAGE_SIZE;
        // No need to set bit 63 because this buffer is visible to VTL2 only.
        let file_offset = gpa;

        tracing::trace!(gpa, file_offset, len, "mapping dma buffer");
        mapping
            .map_file(0, len, gpa_fd.get(), file_offset, true)
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

    #[test]
    fn test_fixed_restore() {
        let state = vec![State::Free {
            base_pfn: 10,
            size_pages: 12,
        }];
        let alloc = FixedPoolAllocator {
            inner: Arc::new(Mutex::new(FixedPoolInner { state })),
        };

        let r1 = alloc
            .restore(
                13.try_into().unwrap(),
                1.try_into().unwrap(),
                "restore1".into(),
            )
            .unwrap();
        assert_eq!(r1.base_pfn, 13);
        assert_eq!(r1.size_pages, 1);

        let r2 = alloc
            .restore(
                15.try_into().unwrap(),
                2.try_into().unwrap(),
                "restore2".into(),
            )
            .unwrap();
        assert_eq!(r2.base_pfn, 15);
        assert_eq!(r2.size_pages, 2);

        let r3 = alloc
            .restore(
                18.try_into().unwrap(),
                4.try_into().unwrap(),
                "restore2".into(),
            )
            .unwrap();
        assert_eq!(r3.base_pfn, 18);
        assert_eq!(r3.size_pages, 4);

        let r4 = alloc
            .restore(
                10.try_into().unwrap(),
                3.try_into().unwrap(),
                "restore2".into(),
            )
            .unwrap();
        assert_eq!(r4.base_pfn, 10);
        assert_eq!(r4.size_pages, 3);

        let r5 = alloc
            .restore(
                14.try_into().unwrap(),
                1.try_into().unwrap(),
                "restore2".into(),
            )
            .unwrap();
        assert_eq!(r5.base_pfn, 14);
        assert_eq!(r5.size_pages, 1);

        assert!(alloc
            .restore(
                5.try_into().unwrap(),
                3.try_into().unwrap(),
                "failed".into()
            )
            .is_err());
        assert!(alloc
            .restore(
                100.try_into().unwrap(),
                10.try_into().unwrap(),
                "failed".into()
            )
            .is_err());
        assert!(alloc
            .restore(
                12.try_into().unwrap(),
                4.try_into().unwrap(),
                "failed".into()
            )
            .is_err());

        let inner = alloc.inner.lock();
        assert_eq!(inner.state.len(), 6);
        // Must be dropped to avoid deadlock after.
        drop(inner);

        drop(r1);
        drop(r2);
        drop(r3);
        drop(r4);
        drop(r5);
    }
}
