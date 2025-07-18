// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides a global DMA manager and client implementation for
//! OpenHCL. The global manager owns the regions used to allocate DMA buffers
//! and provides clients with access to these buffers.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

use anyhow::Context;
use hcl_mapper::HclMapper;
use inspect::Inspect;
use lower_vtl_permissions_guard::LowerVtlMemorySpawner;
use memory_range::MemoryRange;
use page_pool_alloc::PagePool;
use page_pool_alloc::PagePoolAllocator;
use page_pool_alloc::PagePoolAllocatorSpawner;
use parking_lot::Mutex;
use std::sync::Arc;
use thiserror::Error;
use user_driver::DmaClient;
use user_driver::DmaClientAllocStats;
use user_driver::lockmem::LockedMemorySpawner;

/// DMA manager errors.
#[derive(Debug, Error)]
pub enum DmaManagerError {
    /// No memory.
    #[error("no memory")]
    NoMemory,
}

/// Save restore support for [`OpenhclDmaManager`].
pub mod save_restore {
    use super::OpenhclDmaManager;
    use mesh::payload::Protobuf;
    use page_pool_alloc::save_restore::PagePoolState;
    use vmcore::save_restore::RestoreError;
    use vmcore::save_restore::SaveError;
    use vmcore::save_restore::SaveRestore;

    /// The saved state for [`OpenhclDmaManager`].
    #[derive(Protobuf)]
    #[mesh(package = "openhcl.openhcldmamanager")]
    pub struct OpenhclDmaManagerState {
        #[mesh(1)]
        shared_pool: Option<PagePoolState>,
        #[mesh(2)]
        private_pool: Option<PagePoolState>,
    }

    impl SaveRestore for OpenhclDmaManager {
        type SavedState = OpenhclDmaManagerState;

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

            Ok(OpenhclDmaManagerState {
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
                    )));
                }
                (None, Some(_)) => {
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
                    )));
                }
                (None, Some(_)) => {
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
pub struct OpenhclDmaManager {
    /// Page pool with pages that are mapped with shared visibility on CVMs.
    shared_pool: Option<PagePool>,
    /// Page pool with pages that are mapped with private visibility on CVMs.
    private_pool: Option<PagePool>,
    #[inspect(skip)]
    inner: Arc<DmaManagerInner>,
}

/// The required VTL permissions on DMA allocations.
#[derive(Clone, Inspect)]
pub enum LowerVtlPermissionPolicy {
    /// No specific permission constraints are required.
    Any,
    /// All allocations must be accessible to VTL0.
    Vtl0,
}

/// The CVM page visibility required for DMA allocations.
#[derive(Copy, Clone, Inspect)]
pub enum AllocationVisibility {
    /// Allocations must be shared aka host visible.
    Shared,
    /// Allocations must be private.
    Private,
}

/// Client parameters for a new [`OpenhclDmaClient`].
#[derive(Inspect)]
pub struct DmaClientParameters {
    /// The name for this client.
    pub device_name: String,
    /// The required VTL permissions on allocations.
    pub lower_vtl_policy: LowerVtlPermissionPolicy,
    /// The required CVM page visibility for allocations.
    pub allocation_visibility: AllocationVisibility,
    /// Whether allocations must be persistent.
    pub persistent_allocations: bool,
}

struct DmaManagerInner {
    shared_spawner: Option<PagePoolAllocatorSpawner>,
    private_spawner: Option<PagePoolAllocatorSpawner>,
    lower_vtl: Option<Arc<DmaManagerLowerVtl>>,
}

/// Used by [`OpenhclDmaManager`] to modify VTL permissions via
/// [`LowerVtlMemorySpawner`].
///
/// This is required due to some users (like the GET or partition struct itself)
/// that are constructed before the partition struct which normally implements
/// this trait.
///
/// This type should never be created on a hardware isolated VM, as the
/// hypervisor is untrusted.
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

impl DmaManagerInner {
    fn new_dma_client(
        &self,
        params: DmaClientParameters,
        fallback: Option<Arc<OpenhclDmaClient>>,
    ) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        // Allocate the inner client that actually performs the allocations.
        let backing = {
            let DmaClientParameters {
                device_name,
                lower_vtl_policy,
                allocation_visibility,
                persistent_allocations,
            } = &params;

            struct ClientCreation<'a> {
                allocation_visibility: AllocationVisibility,
                persistent_allocations: bool,
                shared_spawner: Option<&'a PagePoolAllocatorSpawner>,
                private_spawner: Option<&'a PagePoolAllocatorSpawner>,
            }

            let creation = ClientCreation {
                allocation_visibility: *allocation_visibility,
                persistent_allocations: *persistent_allocations,
                shared_spawner: self.shared_spawner.as_ref(),
                private_spawner: self.private_spawner.as_ref(),
            };

            match creation {
                ClientCreation {
                    allocation_visibility: AllocationVisibility::Shared,
                    persistent_allocations: _,
                    shared_spawner: Some(shared),
                    private_spawner: _,
                } => {
                    // The shared pool is used by default if available, or if
                    // explicitly requested. All pages are accessible by all
                    // VTLs, so no modification of VTL permissions are required
                    // regardless of what the caller has asked for.
                    DmaClientBacking::SharedPool(
                        shared
                            .allocator(device_name.into())
                            .context("failed to create shared allocator")?,
                    )
                }
                ClientCreation {
                    allocation_visibility: AllocationVisibility::Shared,
                    persistent_allocations: _,
                    shared_spawner: None,
                    private_spawner: _,
                } => {
                    // No sources available that support shared visibility.
                    anyhow::bail!("no sources available for shared visibility")
                }
                ClientCreation {
                    allocation_visibility: AllocationVisibility::Private,
                    persistent_allocations: true,
                    shared_spawner: _,
                    private_spawner: Some(private),
                } => match lower_vtl_policy {
                    LowerVtlPermissionPolicy::Any => {
                        // Only the private pool supports persistent
                        // allocations.
                        DmaClientBacking::PrivatePool(
                            private
                                .allocator(device_name.into())
                                .context("failed to create private allocator")?,
                        )
                    }
                    LowerVtlPermissionPolicy::Vtl0 => {
                        // Private memory must be wrapped in a lower VTL memory
                        // spawner, as otherwise it is accessible to VTL2 only.
                        DmaClientBacking::PrivatePoolLowerVtl(LowerVtlMemorySpawner::new(
                            private
                                .allocator(device_name.into())
                                .context("failed to create private allocator")?,
                            self.lower_vtl
                                .as_ref()
                                .ok_or(anyhow::anyhow!(
                                    "lower vtl not available on hardware isolated platforms"
                                ))?
                                .clone(),
                        ))
                    }
                },
                ClientCreation {
                    allocation_visibility: AllocationVisibility::Private,
                    persistent_allocations: true,
                    shared_spawner: _,
                    private_spawner: None,
                } => {
                    // No sources available that support private persistence.
                    anyhow::bail!("no sources available for private persistent allocations")
                }
                ClientCreation {
                    allocation_visibility: AllocationVisibility::Private,
                    persistent_allocations: false,
                    shared_spawner: _,
                    private_spawner: _,
                } => match lower_vtl_policy {
                    LowerVtlPermissionPolicy::Any => {
                        // No persistence needed means the `LockedMemorySpawner`
                        // using normal VTL2 ram is fine.
                        DmaClientBacking::LockedMemory(LockedMemorySpawner::new())
                    }
                    LowerVtlPermissionPolicy::Vtl0 => {
                        // `LockedMemorySpawner` uses private VTL2 ram, so
                        // lowering VTL permissions is required.
                        DmaClientBacking::LockedMemoryLowerVtl(LowerVtlMemorySpawner::new(
                            LockedMemorySpawner::new(),
                            self.lower_vtl
                                .as_ref()
                                .ok_or(anyhow::anyhow!(
                                    "lower vtl not available on hardware isolated platforms"
                                ))?
                                .clone(),
                        ))
                    }
                },
            }
        };

        Ok(Arc::new(OpenhclDmaClient {
            backing,
            params,
            fallback,
            inner_stats: Mutex::new(DmaClientAllocStats {
                total_alloc: 0,
                fallback_alloc: 0,
            }),
        }))
    }
}

impl OpenhclDmaManager {
    /// Creates a new [`OpenhclDmaManager`] with the given ranges to use for the
    /// shared and private gpa pools.
    pub fn new(
        shared_ranges: &[MemoryRange],
        private_ranges: &[MemoryRange],
        vtom: u64,
        isolation_type: virt::IsolationType,
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

        Ok(OpenhclDmaManager {
            inner: Arc::new(DmaManagerInner {
                shared_spawner: shared_pool.as_ref().map(|pool| pool.allocator_spawner()),
                private_spawner: private_pool.as_ref().map(|pool| pool.allocator_spawner()),
                lower_vtl: if isolation_type.is_hardware_isolated() {
                    None
                } else {
                    Some(DmaManagerLowerVtl::new().context("failed to create lower vtl")?)
                },
            }),
            shared_pool,
            private_pool,
        })
    }

    /// Creates a new DMA client with the given device name and lower VTL
    /// policy.
    pub fn new_client(
        &self,
        params: DmaClientParameters,
        fallback_params: Option<DmaClientParameters>,
    ) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        let fb = if let Some(fb1) = fallback_params {
            self.inner.new_dma_client(fb1, None).ok()
        } else {
            None
        };
        self.inner.new_dma_client(params, fb)
    }

    /// Returns a [`DmaClientSpawner`] for creating DMA clients.
    pub fn client_spawner(&self) -> DmaClientSpawner {
        DmaClientSpawner {
            inner: self.inner.clone(),
        }
    }

    /// Validate restore for the global DMA manager.
    pub fn validate_restore(&self) -> anyhow::Result<()> {
        // Finalize restore for any available pools. Do not allow leaking any
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

    /// Return shared pool size in bytes.
    pub fn shared_pool_size(&self) -> u64 {
        self.shared_pool
            .as_ref()
            .map_or(0, |pool| pool.total_size())
    }

    /// Return private pool size in bytes.
    pub fn private_pool_size(&self) -> u64 {
        self.private_pool
            .as_ref()
            .map_or(0, |pool| pool.total_size())
    }
}

/// A spawner for creating DMA clients.
#[derive(Clone)]
pub struct DmaClientSpawner {
    inner: Arc<DmaManagerInner>,
}

impl DmaClientSpawner {
    /// Creates a new DMA client with the given parameters.
    pub fn new_client(
        &self,
        params: DmaClientParameters,
        fallback_params: Option<DmaClientParameters>,
    ) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        let fb = if let Some(fb1) = fallback_params {
            self.inner.new_dma_client(fb1, None).ok()
        } else {
            None
        };
        self.inner.new_dma_client(params, fb)
    }
}

/// The backing for allocations for an individual dma client. This is used so
/// clients can be inspected to see what actually is backing their allocations.
#[derive(Inspect)]
#[inspect(tag = "type")]
enum DmaClientBacking {
    SharedPool(#[inspect(skip)] PagePoolAllocator),
    PrivatePool(#[inspect(skip)] PagePoolAllocator),
    LockedMemory(#[inspect(skip)] LockedMemorySpawner),
    PrivatePoolLowerVtl(#[inspect(skip)] LowerVtlMemorySpawner<PagePoolAllocator>),
    LockedMemoryLowerVtl(#[inspect(skip)] LowerVtlMemorySpawner<LockedMemorySpawner>),
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
            DmaClientBacking::PrivatePoolLowerVtl(spawner) => {
                spawner.allocate_dma_buffer(total_size)
            }
            DmaClientBacking::LockedMemoryLowerVtl(spawner) => {
                spawner.allocate_dma_buffer(total_size)
            }
        }
    }

    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>> {
        match self {
            DmaClientBacking::SharedPool(allocator) => allocator.attach_pending_buffers(),
            DmaClientBacking::PrivatePool(allocator) => allocator.attach_pending_buffers(),
            DmaClientBacking::LockedMemory(spawner) => spawner.attach_pending_buffers(),
            DmaClientBacking::PrivatePoolLowerVtl(spawner) => spawner.attach_pending_buffers(),
            DmaClientBacking::LockedMemoryLowerVtl(spawner) => spawner.attach_pending_buffers(),
        }
    }

    fn is_persistent(&self) -> bool {
        match self {
            DmaClientBacking::SharedPool(_allocator) => false,
            DmaClientBacking::PrivatePool(_allocator) => true,
            DmaClientBacking::LockedMemory(_spawner) => false,
            DmaClientBacking::PrivatePoolLowerVtl(_spawner) => false,
            DmaClientBacking::LockedMemoryLowerVtl(_spawner) => false,
        }
    }
}

/// An OpenHCL dma client. This client implements inspect to allow seeing what
/// policy and backing is used for this client.
#[derive(Inspect)]
pub struct OpenhclDmaClient {
    backing: DmaClientBacking,
    params: DmaClientParameters,
    #[inspect(skip)] // TODO: Skip for now
    /// Allocation statistics per client.
    inner_stats: Mutex<DmaClientAllocStats>,
    fallback: Option<Arc<OpenhclDmaClient>>,
}

impl DmaClient for OpenhclDmaClient {
    fn allocate_dma_buffer(
        &self,
        total_size: usize,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        // The stats must be tracked here, not in the backing.
        let mut stats = self.inner_stats.lock();
        stats.total_alloc += total_size as u64;
        let mem_block = self.backing.allocate_dma_buffer(total_size).or_else(|_| {
            stats.fallback_alloc += total_size as u64;
            self.fallback
                .as_ref()
                .map_or(Err(DmaManagerError::NoMemory.into()), |f| {
                    f.allocate_dma_buffer(total_size)
                })
        });

        mem_block
    }

    fn attach_pending_buffers(&self) -> anyhow::Result<Vec<user_driver::memory::MemoryBlock>> {
        self.backing.attach_pending_buffers()
    }

    /// Query if this client supports persistent allocations.
    fn is_persistent(&self) -> bool {
        self.backing.is_persistent()
    }

    /// How much memory was allocated during session.
    fn alloc_size(&self) -> u64 {
        self.inner_stats.lock().total_alloc
    }

    /// How much backup memory was allocated during session (fallback).
    fn fallback_alloc_size(&self) -> u64 {
        self.inner_stats.lock().fallback_alloc
    }
}
