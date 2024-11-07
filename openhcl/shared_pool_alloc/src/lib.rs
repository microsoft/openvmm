// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module implements a shared memory allocator for allocating shared pages
//! on isolated platforms.

#![cfg(unix)]
#![warn(missing_docs)]

mod device_dma;

pub use device_dma::SharedDmaBuffer;

#[cfg(feature = "vfio")]
use anyhow::Context;
use hvdef::HV_PAGE_SIZE;
use inspect::Inspect;
use parking_lot::Mutex;
use std::num::NonZeroU64;
use std::sync::Arc;
use thiserror::Error;
use vm_topology::memory::MemoryRangeWithNode;

/// Error returned when unable to allocate memory.
#[derive(Debug, Error)]
#[error("unable to allocate shared pool size {size} with tag {tag}")]
pub struct SharedPoolOutOfMemory {
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
        pfn_bias: u64,
        #[inspect(hex)]
        size_pages: u64,
    },
    Allocated {
        #[inspect(hex)]
        base_pfn: u64,
        #[inspect(hex)]
        pfn_bias: u64,
        #[inspect(hex)]
        size_pages: u64,
        tag: String,
    },
}

#[derive(Inspect, Debug)]
struct SharedPoolInner {
    #[inspect(iter_by_index)]
    state: Vec<State>,
}

/// A handle for a shared pool allocation. When dropped, the allocation is
/// freed.
#[derive(Debug)]
pub struct SharedPoolHandle {
    inner: Arc<Mutex<SharedPoolInner>>,
    base_pfn: u64,
    pfn_bias: u64,
    size_pages: u64,
}

impl SharedPoolHandle {
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

impl Drop for SharedPoolHandle {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();

        let index = inner
            .state
            .iter()
            .position(|state| {
                if let State::Allocated {
                    base_pfn: base,
                    pfn_bias: offset,
                    size_pages: len,
                    tag: _,
                } = state
                {
                    *base == self.base_pfn && *offset == self.pfn_bias && *len == self.size_pages
                } else {
                    false
                }
            })
            .expect("must find allocation");

        inner.state[index] = State::Free {
            base_pfn: self.base_pfn,
            pfn_bias: self.pfn_bias,
            size_pages: self.size_pages,
        };
    }
}

/// A page allocator for shared memory.
///
/// Pages are allocated via [`SharedPoolAllocator`] from [`Self::allocator`].
///
/// This struct is considered the "owner" of the pool allowing for save/restore.
///
// TODO SNP: Implement save restore. This means additionally having some sort of
// restore_alloc method that maps to an existing allocation.
#[derive(Inspect)]
pub struct SharedPool {
    #[inspect(flatten)]
    inner: Arc<Mutex<SharedPoolInner>>,
}

impl SharedPool {
    /// Create a shared pool allocator, with the specified memory. The supplied
    /// memory is assumed to shared (and therefore unaccepted on SNP).
    ///
    /// `addr_bias` represents a bias to apply to addresses in `shared_pool`.
    /// This should be vtom on hardware isolated platforms.
    pub fn new(shared_pool: &[MemoryRangeWithNode], addr_bias: u64) -> anyhow::Result<Self> {
        let mut pages = Vec::new();
        for range in shared_pool {
            pages.push(State::Free {
                base_pfn: range.range.start() / HV_PAGE_SIZE,
                pfn_bias: addr_bias / HV_PAGE_SIZE,
                size_pages: range.range.len() / HV_PAGE_SIZE,
            });
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(SharedPoolInner { state: pages })),
        })
    }

    /// Create an allocator instance that can be used to allocate pages.
    pub fn allocator(&self) -> SharedPoolAllocator {
        SharedPoolAllocator {
            inner: self.inner.clone(),
        }
    }

    // TODO: save method and restore
}

/// A page allocator for shared memory.
///
/// Pages are allocated via the [`Self::alloc`] method and freed by dropping the
/// associated handle returned.
#[derive(Clone, Debug)]
pub struct SharedPoolAllocator {
    inner: Arc<Mutex<SharedPoolInner>>,
}

impl SharedPoolAllocator {
    /// Allocate contiguous pages from the shared visibility pool with the given
    /// tag. If a contiguous region of free pages is not available, then an
    /// error is returned.
    pub fn alloc(
        &self,
        size_pages: NonZeroU64,
        tag: String,
    ) -> Result<SharedPoolHandle, SharedPoolOutOfMemory> {
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
            .ok_or(SharedPoolOutOfMemory {
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

        Ok(SharedPoolHandle {
            inner: self.inner.clone(),
            base_pfn,
            pfn_bias,
            size_pages,
        })
    }
}

#[cfg(feature = "vfio")]
impl user_driver::vfio::VfioDmaBuffer for SharedPoolAllocator {
    fn create_dma_buffer(&self, len: usize) -> anyhow::Result<user_driver::memory::MemoryBlock> {
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
                "vfio dma".into(),
            )
            .context("failed to allocate shared mem")?;

        let gpa_fd = hcl::ioctl::MshvVtlLow::new().context("failed to open gpa fd")?;
        let mapping = sparse_mmap::SparseMapping::new(len).context("failed to create mapping")?;

        let gpa = alloc.base_pfn() * HV_PAGE_SIZE;

        // On hardware isolated platforms, the file_offset must have bit 63 set
        // as these are decrypted pages. Setting this bit is okay on
        // non-hardware isolated platforms, as it does nothing.
        let file_offset = gpa | (1 << 63);

        tracing::trace!(gpa, file_offset, len, "mapping dma buffer");
        mapping
            .map_file(0, len, gpa_fd.get(), file_offset, true)
            .context("unable to map allocation")?;

        let pfns: Vec<_> = (alloc.base_pfn()..alloc.base_pfn() + alloc.size_pages).collect();

        Ok(user_driver::memory::MemoryBlock::new(SharedDmaBuffer {
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
    fn test_basic_alloc() {
        let state = vec![State::Free {
            base_pfn: 10,
            pfn_bias: 15,
            size_pages: 20,
        }];
        let alloc = SharedPoolAllocator {
            inner: Arc::new(Mutex::new(SharedPoolInner { state })),
        };

        let a1 = alloc.alloc(5.try_into().unwrap(), "alloc1".into()).unwrap();
        assert_eq!(a1.base_pfn, 10);
        assert_eq!(a1.pfn_bias, 15);
        assert_eq!(a1.base_pfn(), a1.base_pfn + a1.pfn_bias);
        assert_eq!(a1.base_pfn_without_bias(), a1.base_pfn);
        assert_eq!(a1.size_pages, 5);

        let a2 = alloc
            .alloc(15.try_into().unwrap(), "alloc2".into())
            .unwrap();
        assert_eq!(a2.base_pfn, 15);
        assert_eq!(a2.pfn_bias, 15);
        assert_eq!(a2.base_pfn(), a2.base_pfn + a2.pfn_bias);
        assert_eq!(a2.base_pfn_without_bias(), a2.base_pfn);
        assert_eq!(a2.size_pages, 15);

        assert!(alloc.alloc(1.try_into().unwrap(), "failed".into()).is_err());

        drop(a1);
        drop(a2);

        let inner = alloc.inner.lock();
        assert_eq!(inner.state.len(), 2);
    }
}
