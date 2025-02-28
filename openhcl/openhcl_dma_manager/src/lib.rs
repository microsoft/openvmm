// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides a global DMA manager and client implementation for
//! OpenHCL. The global manager owns the regions used to allocate DMA buffers
//! and provides clients with access to these buffers.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
#![warn(missing_docs)]

use anyhow::Context;
use guestmem::ranges::PagedRange;
use guestmem::GuestMemory;
use hcl_mapper::HclMapper;
use inspect::Inspect;
use lower_vtl_permissions_guard::LowerVtlMemorySpawner;
use memory_range::MemoryRange;
use page_pool_alloc::PagePool;
use page_pool_alloc::PagePoolAllocator;
use page_pool_alloc::PagePoolAllocatorSpawner;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use user_driver::lockmem::LockedMemorySpawner;
use user_driver::memory::PAGE_SIZE;
use user_driver::memory::PAGE_SIZE64;
use user_driver::page_allocator::PageAllocator;
use user_driver::DmaClient;
use user_driver::DmaOperation;
use user_driver::DmaTransaction;
use user_driver::MapDmaError;
use user_driver::MapDmaOptions;

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
                    )))
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
                    )))
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
#[derive(Inspect)]
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
    /// Whether to allocate a bounce buffer for this client.
    pub bounce_buffer_pages: Option<u64>,
}

struct DmaManagerInner {
    shared_spawner: Option<PagePoolAllocatorSpawner>,
    private_spawner: Option<PagePoolAllocatorSpawner>,
    lower_vtl: Arc<DmaManagerLowerVtl>,
    pin_pages: PinPages,
    // TODO: must track existing mapped dma ranges for save/restore
}

/// Used by [`OpenhclDmaManager`] to modify VTL permissions via
/// [`LowerVtlMemorySpawner`].
///
/// This is required due to some users (like the GET or partition struct itself)
/// that are constructed before the partition struct which normally implements
/// this trait.
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

struct PinPages {
    mshv_hvcall: hcl::ioctl::MshvHvcall,
    // TODO: have some way of looking up which ranges are pre-pinned or not.
    // prepinned_ranges: bool,
}

impl PinPages {
    fn new() -> anyhow::Result<Self> {
        let mshv_hvcall = hcl::ioctl::MshvHvcall::new().context("failed to open mshv_hvcall")?;
        mshv_hvcall.set_allowed_hypercalls(&[
            hvdef::HypercallCode::HvCallPinGpaPageRanges,
            hvdef::HypercallCode::HvCallUnpinGpaPageRanges,
        ]);
        Ok(Self { mshv_hvcall })
    }

    /// Check if all the pages are pinned.
    fn is_pinned(&self, pfns: &[u64]) -> bool {
        true
    }

    /// returns true if successful pin, false otherwise
    #[must_use]
    fn pin_pages(&self, pfns: &[u64]) -> bool {
        // TODO: impl
        true
    }

    fn unpin_pages(&self, pfns: &[u64]) {
        // TODO: impl
    }
}

impl DmaManagerInner {
    fn new_dma_client(
        self: &Arc<Self>,
        params: DmaClientParameters,
    ) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        // Allocate the inner client that actually performs the allocations.
        let backing = {
            let DmaClientParameters {
                device_name,
                lower_vtl_policy,
                allocation_visibility,
                persistent_allocations,
                bounce_buffer_pages: _,
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
                            self.lower_vtl.clone(),
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
                        DmaClientBacking::LockedMemory(LockedMemorySpawner)
                    }
                    LowerVtlPermissionPolicy::Vtl0 => {
                        // `LockedMemorySpawner` uses private VTL2 ram, so
                        // lowering VTL permissions is required.
                        DmaClientBacking::LockedMemoryLowerVtl(LowerVtlMemorySpawner::new(
                            LockedMemorySpawner,
                            self.lower_vtl.clone(),
                        ))
                    }
                },
            }
        };

        // Allocate the bounce buffer from the backing for the client.
        //
        // TODO: req to use persistent pools?
        let bounce_pfns = if let Some(pages) = params.bounce_buffer_pages {
            let pages = backing
                .allocate_dma_buffer((pages * PAGE_SIZE64) as usize)
                .context(format!("unable to allocate bounce buffer {pages} pages"))?;

            // Pin the bounce buffer pages, if required.
            if !self.pin_pages.is_pinned(pages.pfns()) {
                if !self.pin_pages.pin_pages(pages.pfns()) {
                    anyhow::bail!("unable to pin bounce buffer pages");
                }
            }

            Some(PageAllocator::new(pages))
        } else {
            None
        };

        Ok(Arc::new(OpenhclDmaClient {
            inner: self.clone(),
            backing,
            params,
            bounce_pfns,
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
                lower_vtl: DmaManagerLowerVtl::new().context("failed to create lower vtl")?,
                pin_pages: PinPages::new().context("failed to create pin pages")?,
            }),
            shared_pool,
            private_pool,
        })
    }

    /// Creates a new DMA client with the given device name and lower VTL
    /// policy.
    pub fn new_client(&self, params: DmaClientParameters) -> anyhow::Result<Arc<OpenhclDmaClient>> {
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
}

/// A spawner for creating DMA clients.
#[derive(Clone)]
pub struct DmaClientSpawner {
    inner: Arc<DmaManagerInner>,
}

impl DmaClientSpawner {
    /// Creates a new DMA client with the given parameters.
    pub fn new_client(&self, params: DmaClientParameters) -> anyhow::Result<Arc<OpenhclDmaClient>> {
        self.inner.new_dma_client(params)
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

    fn attach_dma_buffer(
        &self,
        len: usize,
        base_pfn: u64,
    ) -> anyhow::Result<user_driver::memory::MemoryBlock> {
        match self {
            DmaClientBacking::SharedPool(allocator) => allocator.attach_dma_buffer(len, base_pfn),
            DmaClientBacking::PrivatePool(allocator) => allocator.attach_dma_buffer(len, base_pfn),
            DmaClientBacking::LockedMemory(spawner) => spawner.attach_dma_buffer(len, base_pfn),
            DmaClientBacking::PrivatePoolLowerVtl(spawner) => {
                spawner.attach_dma_buffer(len, base_pfn)
            }
            DmaClientBacking::LockedMemoryLowerVtl(spawner) => {
                spawner.attach_dma_buffer(len, base_pfn)
            }
        }
    }
}

/// An OpenHCL dma client. This client implements inspect to allow seeing what
/// policy and backing is used for this client.
#[derive(Inspect)]
pub struct OpenhclDmaClient {
    #[inspect(skip)]
    inner: Arc<DmaManagerInner>,
    backing: DmaClientBacking,
    params: DmaClientParameters,
    bounce_pfns: Option<PageAllocator>,
}

impl OpenhclDmaClient {
    /// Allocate bounce pages and prepare the bounce buffers with the required
    /// data.
    async fn allocate_bounce_pages<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Result<DmaOperation<'a>, MapDmaError> {
        // TODO: nonblocking mode return immediately on allocation failure

        // allocate bounce buffer
        let bounce_pages = self
            .bounce_pfns
            .as_ref()
            .ok_or(MapDmaError::NoBounceBufferAvailable)?
            .alloc_pages(range.gpns().len())
            .await
            .expect("BUGBUG more bouncing required than pages available");

        // copy to bounced pages
        if options.is_tx {
            for (index, page) in range.gpns().iter().enumerate() {
                let bounce_page = bounce_pages.page_as_slice(index);

                // BUGBUG: does not handle subranges, copies whole pages.
                // there's not a good method for this in PagedRange, needs some
                // thinking.
                guest_memory
                    .read_to_atomic(*page * PAGE_SIZE64, bounce_page)
                    .expect("BUGBUG handle bounce copy error");
            }
        }

        Ok(DmaOperation::Bounced(bounce_pages, range))
    }

    async fn map_dma_ranges_inner<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Result<DmaTransaction<'a>, MapDmaError> {
        // BUGBUG: does not handle if the first/last page are partial pages. Do we allow this?
        assert_eq!(range.offset(), 0);
        assert_eq!(range.len() % PAGE_SIZE, 0);

        // the transaction is either all bounced, or all pinned.
        let operation = if options.always_bounce {
            self.allocate_bounce_pages(guest_memory, range, options)
                .await?
        } else if self.inner.pin_pages.is_pinned(range.gpns()) {
            DmaOperation::PrePinned(range)
        } else {
            if self.inner.pin_pages.pin_pages(range.gpns()) {
                DmaOperation::Pinned(range)
            } else {
                self.allocate_bounce_pages(guest_memory, range, options)
                    .await?
            }
        };

        Ok(DmaTransaction {
            guest_memory,
            operation,
            options,
        })
    }
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

    fn map_dma_ranges<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Pin<Box<dyn Future<Output = Result<DmaTransaction<'a>, MapDmaError>> + 'a>> {
        Box::pin(self.map_dma_ranges_inner(guest_memory, range, options))
    }

    fn unmap_dma_ranges(&self, transaction: DmaTransaction<'_>) -> Result<(), MapDmaError> {
        match transaction.operation {
            DmaOperation::PrePinned(_) => {}
            DmaOperation::Pinned(range) => {
                self.inner.pin_pages.unpin_pages(range.gpns());
            }
            DmaOperation::Bounced(bounce_pages, range) => {
                // BUGBUG assumes no partial pages in range
                if transaction.options.is_rx || transaction.options.always_bounce {
                    for (index, page) in range.gpns().iter().enumerate() {
                        let bounce_page = bounce_pages.page_as_slice(index);

                        // BUGBUG: does not handle subranges, copies whole pages.
                        // there's not a good method for this in PagedRange, needs some
                        // thinking.
                        transaction
                            .guest_memory
                            .write_from_atomic(*page * PAGE_SIZE64, bounce_page)
                            .expect("BUGBUG handle bounce copy error");
                    }
                }
            }
        }

        Ok(())
    }
}
