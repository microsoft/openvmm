// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides a global DMA manager and client implementation. The
//! global manager owns the regions used to allocate DMA buffers and provides
//! clients with access to these buffers.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::Context;
use hcl_mapper::HclMapper;
use inspect::Inspect;
use lower_vtl_permissions_guard::LowerVtlMemorySpawner;
use memory_range::MemoryRange;
use page_pool_alloc::PagePool;
use page_pool_alloc::PagePoolAllocator;
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

/// A global DMA manager that owns various pools of memory for managing
/// buffers and clients using DMA.
#[derive(Inspect)]
pub struct GlobalDmaManager {
    /// Page pool with pages that are mapped with shared visibility on CVMs.
    shared_pool: Option<PagePool>,
    /// Page pool with pages that are mapped with private visibility on CVMs.
    private_pool: Option<PagePool>,
    #[inspect(skip)]
    inner: Arc<DmaManagerInner>,
}

#[derive(Inspect)]
pub enum LowerVtlPermissionPolicy {
    Default,
    Vtl0,
}

/// The CVM page visibility required for DMA allocations.
#[derive(Copy, Clone, Inspect)]
pub enum AllocationVisibility {
    Default,
    Shared,
    Private,
}

#[derive(Inspect)]
pub struct DmaClientParameters {
    pub device_name: String,
    pub lower_vtl_policy: LowerVtlPermissionPolicy,
    pub allocation_visibility: AllocationVisibility,
    pub persistent_allocations: bool,
}

pub struct DmaManagerInner {
    shared_spawner: Option<PagePoolAllocatorSpawner>,
    private_spawner: Option<PagePoolAllocatorSpawner>,
    lower_vtl: Arc<DmaManagerLowerVtl>,
}

/// Used by [`GlobalDmaManager`] to modify VTL permissions via
/// [`LowerVtlMemorySpawner`].
///
/// This is required due to devices (like the GET) that unfortunately are
/// constructed before the partition struct which normally implements this
/// trait.
struct DmaManagerLowerVtl {
    mshv_hvcall: hcl::ioctl::MshvHvcall,
}

impl DmaManagerLowerVtl {
    pub fn new() -> anyhow::Result<Arc<Self>> {
        let mshv_hvcall = hcl::ioctl::MshvHvcall::new().context("failed to open mshv_hvcall")?;
        mshv_hvcall.set_allowed_hypercalls(&[hvdef::HypercallCode::HvCallModifyVtlProtectionMask]);
        Ok(Arc::new(Self { mshv_hvcall }))
    }
}

impl virt::VtlMemoryProtection for DmaManagerLowerVtl {
    fn modify_vtl_page_setting(&self, pfn: u64, flags: hvdef::HvMapGpaFlags) -> anyhow::Result<()> {
        self.mshv_hvcall
            .modify_vtl_protection_mask(
                MemoryRange::from_4k_gpn_range(pfn..pfn + 1),
                flags,
                hvdef::hypercall::HvInputVtl::CURRENT_VTL,
            )
            .context("failed to modify VTL page permissions")
    }
}

/// Wrap a `dma_client` in a [`LowerVtlMemorySpawner`] if the `policy` requires
/// it. This assumes that `dma_client` requires lowering permission for
/// allocated buffers.
fn wrap_in_lower_vtl(
    dma_client: impl DmaClient + 'static,
    policy: LowerVtlPermissionPolicy,
    lower_vtl: &Arc<DmaManagerLowerVtl>,
) -> anyhow::Result<Arc<dyn DmaClient>> {
    match policy {
        LowerVtlPermissionPolicy::Default => Ok(Arc::new(dma_client)),
        LowerVtlPermissionPolicy::Vtl0 => {
            // Private memory must be wrapped in a lower VTL memory spawner, as
            // otherwise it is accessible to VTL2 only.
            Ok(Arc::new(LowerVtlMemorySpawner::new(
                dma_client,
                lower_vtl.clone(),
            )))
        }
    }
}

impl DmaManagerInner {
    // // BUGBUG return wrapped dma client that implements identification via inspect about policy and backing used for allocations
    // fn new_dma_client(&self, params: &DmaClientParameters) -> anyhow::Result<Arc<dyn DmaClient>> {
    //     let DmaClientParameters {
    //         device_name,
    //         lower_vtl_policy,
    //         allocation_visibility,
    //         persistent_allocations,
    //     } = params;

    //     match (allocation_visibility, self.shared_spawner.as_ref()) {
    //         (AllocationVisibility::Default, Some(spawner))
    //         | (AllocationVisibility::Shared, Some(spawner)) => {
    //             // Shared visibility memory by default has no protections on any
    //             // VTLs, so no modification is required.
    //             return Ok(Arc::new(
    //                 spawner
    //                     .allocator(device_name)
    //                     .context("failed to create shared allocator")?,
    //             ));
    //         }

    //         (AllocationVisibility::Shared, None) => {
    //             // No sources available that support shared visibility.
    //             anyhow::bail!("no sources available for shared visibility")
    //         }

    //         (AllocationVisibility::Private, _) | (AllocationVisibility::Default, _) => {
    //             // This is handled by the match statement below.
    //         }
    //     }

    //     assert!(matches!(
    //         allocation_visibility,
    //         AllocationVisibility::Default | AllocationVisibility::Private
    //     ));

    //     match (persistent_allocations, self.private_spawner.as_ref()) {
    //         (true, Some(pool)) => {
    //             // Persistent allocations are available via the private pool.
    //             let allocator = pool
    //                 .allocator(device_name)
    //                 .context("failed to create private allocator")?;
    //             wrap_in_lower_vtl(allocator, lower_vtl_policy, &self.lower_vtl)
    //         }
    //         (true, None) => {
    //             // No sources available that support persistence.
    //             anyhow::bail!("no sources available for persistent allocations")
    //         }
    //         (false, _) => {
    //             // No persistence needeed means the LockedMemorySpawner using
    //             // normal VTL2 ram is fine.
    //             wrap_in_lower_vtl(LockedMemorySpawner, lower_vtl_policy, &self.lower_vtl)
    //         }
    //     }
    // }

    fn new_dma_client(&self, params: DmaClientParameters) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        // Allocate the inner client that actually performs the allocations.
        let backing = {
            let DmaClientParameters {
                device_name,
                lower_vtl_policy,
                allocation_visibility,
                persistent_allocations,
            } = &params;

            match (
                allocation_visibility,
                persistent_allocations,
                self.shared_spawner.as_ref(),
                self.private_spawner.as_ref(),
            ) {
                (AllocationVisibility::Default, _, Some(shared), _)
                | (AllocationVisibility::Shared, _, Some(shared), _) => {
                    // The shared pool is used by default if available, or if
                    // explicitly requested.
                    DmaClientBacking::SharedPool(
                        shared
                            .allocator(device_name.into())
                            .context("failed to create shared allocator")?,
                    )
                }
                (AllocationVisibility::Shared, _, None, _) => {
                    // No sources available that support shared visibility.
                    anyhow::bail!("no sources available for shared visibility")
                }
                (AllocationVisibility::Default, true, None, Some(private))
                | (AllocationVisibility::Private, true, _, Some(private)) => {
                    // Only the private pool supports persistent allocations,
                    // and is used if requested or no shared pool is available.
                    DmaClientBacking::PrivatePool(
                        private
                            .allocator(device_name.into())
                            .context("failed to create private allocator")?,
                    )
                }
                (AllocationVisibility::Private, true, _, None) => {
                    // No sources available that support private persistence.
                    anyhow::bail!("no sources available for private persistent allocations")
                }
                (AllocationVisibility::Private, false, _, _)
                | (AllocationVisibility::Default, false, _, _) => {
                    // No persistence needeed means the LockedMemorySpawner
                    // using normal VTL2 ram is fine.
                    DmaClientBacking::LockedMemory(LockedMemorySpawner)
                }
                (_, true, None, None) => {
                    // No sources available that support persistence.
                    anyhow::bail!("no sources available for persistent allocations")
                }
            }
        };

        Ok(Arc::new(OpenhclDmaClient { backing, params }))
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
                lower_vtl: DmaManagerLowerVtl::new().context("failed to create lower vtl")?,
            }),
            shared_pool,
            private_pool,
        })
    }

    /// Creates a new DMA client with the given device name and lower VTL
    /// policy.
    pub fn new_dma_client(
        &self,
        params: DmaClientParameters,
    ) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        self.inner.new_dma_client(params)
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
        params: DmaClientParameters,
    ) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        self.inner.new_dma_client(params)
    }
}

#[derive(Inspect)]
#[inspect(tag = "type")]
enum DmaClientBacking {
    SharedPool(#[inspect(skip)] PagePoolAllocator),
    PrivatePool(#[inspect(skip)] PagePoolAllocator),
    LockedMemory(#[inspect(skip)] LockedMemorySpawner),
}

impl DmaClientBacking {
    fn allocate_dma_buffer(
        &self,
        total_size: usize,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        match self {
            DmaClientBacking::SharedPool(allocator) => allocator.allocate_dma_buffer(total_size),
            DmaClientBacking::PrivatePool(allocator) => allocator.allocate_dma_buffer(total_size),
            DmaClientBacking::LockedMemory(spawner) => spawner.allocate_dma_buffer(total_size),
        }
    }

    fn attach_dma_buffer(
        &self,
        len: usize,
        base_pfn: u64,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        match self {
            DmaClientBacking::SharedPool(allocator) => allocator.attach_dma_buffer(len, base_pfn),
            DmaClientBacking::PrivatePool(allocator) => allocator.attach_dma_buffer(len, base_pfn),
            DmaClientBacking::LockedMemory(spawner) => spawner.attach_dma_buffer(len, base_pfn),
        }
    }
}

/// An OpenHCL dma client.
#[derive(Inspect)]
pub struct OpenhclDmaClient {
    backing: DmaClientBacking,
    params: DmaClientParameters,
}

impl DmaClient for OpenhclDmaClient {
    fn allocate_dma_buffer(
        &self,
        total_size: usize,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        self.backing.allocate_dma_buffer(total_size)
    }

    fn attach_dma_buffer(
        &self,
        len: usize,
        base_pfn: u64,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        self.backing.attach_dma_buffer(len, base_pfn)
    }
}
