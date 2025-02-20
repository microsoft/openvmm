// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides a global DMA manager and client implementation. The
//! global manager owns the regions used to allocate DMA buffers and provides
//! clients with access to these buffers.

use anyhow::Context;
use hcl_mapper::HclMapper;
use lower_vtl_permissions_guard::LowerVtlMemorySpawner;
use memory_range::MemoryRange;
use page_pool_alloc::PagePool;
use page_pool_alloc::PagePoolAllocatorSpawner;
use std::sync::Arc;
use user_driver::lockmem::LockedMemorySpawner;
use user_driver::DmaClient;

/// Save restore support for [`GlobalDmaManager`].
pub mod save_restore {
    use super::GlobalDmaManager;
    use mesh::payload::Protobuf;
    use page_pool_alloc::save_restore::PagePoolState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    /// The saved state for [`GlobalDmaManager`].
    #[derive(Protobuf)]
    #[mesh(package = "openhcl.globaldmamanager")]
    pub struct GlobalDmaManagerState {
        #[mesh(1)]
        shared_pool: Option<PagePoolState>,
        #[mesh(2)]
        private_pool: Option<PagePoolState>,
    }

    impl SaveRestore for GlobalDmaManager {
        type SavedState = GlobalDmaManagerState;

        fn save(&mut self) -> Result<Self::SavedState, SaveError> {
            let shared_pool = self
                .shared_pool
                .as_mut()
                .map(SaveRestore::save)
                .transpose()
                .map_err(|e| {
                    SaveError::ChildError("shared pool save failed".into(), Box::new(e))
                })?;

            let private_pool = self
                .private_pool
                .as_mut()
                .map(SaveRestore::save)
                .transpose()
                .map_err(|e| {
                    SaveError::ChildError("private pool save failed".into(), Box::new(e))
                })?;

            Ok(GlobalDmaManagerState {
                shared_pool,
                private_pool,
            })
        }

        fn restore(&mut self, state: Self::SavedState) -> Result<(), RestoreError> {
            match (state.shared_pool, self.shared_pool.as_mut()) {
                (None, None) => {}
                (Some(_), None) => {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "saved state for shared pool but no shared pool"
                    )))
                }
                (None, Some(_)) => {
                    // BUGBUG Is this right?
                    // It's possible that previously we did not have a shared
                    // pool, so there may not be any state to restore.
                }
                (Some(state), Some(pool)) => {
                    pool.restore(state).map_err(|e| {
                        RestoreError::ChildError("shared pool restore failed".into(), Box::new(e))
                    })?;
                }
            }

            match (state.private_pool, self.private_pool.as_mut()) {
                (None, None) => {}
                (Some(_), None) => {
                    return Err(RestoreError::InvalidSavedState(anyhow::anyhow!(
                        "saved state for private pool but no private pool"
                    )))
                }
                (None, Some(_)) => {
                    // BUGBUG Is this right?
                    // It's possible that previously we did not have a private
                    // pool, so there may not be any state to restore.
                }
                (Some(state), Some(pool)) => {
                    pool.restore(state).map_err(|e| {
                        RestoreError::ChildError("private pool restore failed".into(), Box::new(e))
                    })?;
                }
            }

            Ok(())
        }
    }
}

pub struct GlobalDmaManager {
    /// Page pool with pages that are mapped with shared visibility on CVMs.
    shared_pool: Option<PagePool>,
    /// Page pool with pages that are mapped with private visibility on CVMs.
    private_pool: Option<PagePool>,
    inner: Arc<DmaManagerInner>,
}

enum LowerVtlPermissionPolicy {
    Default,
    Vtl0,
}

pub struct DmaManagerInner {
    shared_spawner: Option<PagePoolAllocatorSpawner>,
    private_spawner: Option<PagePoolAllocatorSpawner>,
}

impl DmaManagerInner {
    fn new_dma_client(
        &self,
        device_name: String,
        lower_vtl_policy: LowerVtlPermissionPolicy,
    ) -> anyhow::Result<Arc<dyn DmaClient>> {
        if let Some(spawner) = &self.shared_spawner {
            // Shared visibility memory by default has no protections on any
            // VTLs, so no modification is required.
            Ok(Arc::new(
                spawner
                    .allocator(device_name)
                    .context("failed to create shared allocator")?,
            ))
        } else if let Some(spawner) = &self.private_spawner {
            let allocator = spawner
                .allocator(device_name)
                .context("failed to create private allocator")?;
            match lower_vtl_policy {
                LowerVtlPermissionPolicy::Default => Ok(Arc::new(allocator)),
                LowerVtlPermissionPolicy::Vtl0 => {
                    // Private memory must be wrapped in a lower VTL memory
                    // spawner, as otherwise it is accessible to VTL2 only.
                    Ok(Arc::new(LowerVtlMemorySpawner::new(
                        allocator,
                        get_lower_vtl::GetLowerVtl::new()
                            .context("failed to create get lower vtl")?,
                    )))
                }
            }
        } else {
            // No pools available means to use the LockedMemorySpawner.
            Ok(Arc::new(LockedMemorySpawner))
        }
    }
}

impl GlobalDmaManager {
    /// Creates a new `GlobalDmaManager` with the given ranges to use for the
    /// shared and private gpa pools.
    pub fn new(
        shared_ranges: &[MemoryRange],
        private_ranges: &[MemoryRange],
        vtom: u64,
    ) -> anyhow::Result<Self> {
        let shared_pool = if shared_ranges.is_empty() {
            None
        } else {
            Some(
                PagePool::new(
                    shared_ranges,
                    HclMapper::new_shared(vtom).context("failed to create hcl mapper")?,
                )
                .context("failed to create shared page pool")?,
            )
        };

        let private_pool = if private_ranges.is_empty() {
            None
        } else {
            Some(
                PagePool::new(
                    private_ranges,
                    HclMapper::new_private().context("failed to create hcl mapper")?,
                )
                .context("failed to create private page pool")?,
            )
        };

        Ok(GlobalDmaManager {
            inner: Arc::new(DmaManagerInner {
                shared_spawner: shared_pool.as_ref().map(|pool| pool.allocator_spawner()),
                private_spawner: private_pool.as_ref().map(|pool| pool.allocator_spawner()),
            }),
            shared_pool,
            private_pool,
        })
    }

    /// Returns a [`DmaClientSpawner`] for creating DMA clients.
    pub fn client_spawner(&self) -> DmaClientSpawner {
        DmaClientSpawner {
            inner: self.inner.clone(),
        }
    }

    /// Validate restore for the global DMA manager.
    pub fn validate_restore(&self) -> anyhow::Result<()> {
        // Finilize restore for any available pools. Do not allow leaking any
        // allocations.
        if let Some(shared_pool) = &self.shared_pool {
            shared_pool
                .validate_restore(false)
                .context("failed to validate restore for shared pool")?
        }

        if let Some(private_pool) = &self.private_pool {
            private_pool
                .validate_restore(false)
                .context("failed to validate restore for private pool")?
        }

        Ok(())
    }
}

/// A spawner for creating DMA clients.
#[derive(Clone)]
pub struct DmaClientSpawner {
    inner: Arc<DmaManagerInner>,
}

impl DmaClientSpawner {
    /// Creates a new DMA client with the given device name and lower VTL
    /// policy.
    pub fn create_client(
        &self,
        device_name: String,
        lower_vtl_policy: LowerVtlPermissionPolicy,
    ) -> anyhow::Result<Arc<dyn DmaClient>> {
        self.inner.new_dma_client(device_name, lower_vtl_policy)
    }
}
