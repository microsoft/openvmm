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
use guestmem::PAGE_SIZE;
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
use user_driver::memory::PAGE_SIZE64;
use user_driver::page_allocator::PageAllocator;
use user_driver::page_allocator::ScopedPages;
use user_driver::DmaAlloc;
use user_driver::DmaClient;
use user_driver::DmaMap;
use user_driver::MapDmaError;
use user_driver::MapDmaOptions;
use user_driver::MappedDmaTransaction;

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
    pin_pages: Option<PinPages>,
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
    // Today, it's assumed that all pages need pinning.
}

impl std::fmt::Debug for PinPages {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinPages").finish()
    }
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
    fn is_pinned(&self, _pfns: &[u64]) -> bool {
        false
    }

    /// returns true if successful pin, false otherwise
    #[must_use]
    fn pin_pages(&self, pfns: &[u64]) -> bool {
        // TODO: What happens if some pages are already physically backed and
        // some are VA backed? is that valid?
        tracing::trace!(?pfns, "pinning pfns");

        let ranges = pfns
            .iter()
            .map(|pfn| MemoryRange::from_4k_gpn_range(*pfn..*pfn + 1))
            .collect::<Vec<_>>();
        self.mshv_hvcall.pin_gpa_ranges(&ranges).is_ok()
    }

    fn unpin_pages(&self, pfns: &[u64]) {
        tracing::trace!(?pfns, "unpinning pfns");

        let ranges = pfns
            .iter()
            .map(|pfn| MemoryRange::from_4k_gpn_range(*pfn..*pfn + 1))
            .collect::<Vec<_>>();
        self.mshv_hvcall
            .unpin_gpa_ranges(&ranges)
            .expect("unpin cannot fail");
    }
}

impl DmaManagerInner {
    fn new_dma_client(self: &Arc<Self>, params: DmaClientParameters) -> anyhow::Result<DmaClient> {
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

        // Allocate the bounce buffer from the backing for the client. Today, is
        // only supported if pinning is required.
        let bounce_pfns = if let Some(pages) = params.bounce_buffer_pages {
            let pin_pages = self.pin_pages.as_ref().ok_or(anyhow::anyhow!(
                "bounce buffer pages only supported if dma manager supports pinning"
            ))?;

            let block = backing
                .allocate_dma_buffer((pages * PAGE_SIZE64) as usize)
                .context(format!("unable to allocate bounce buffer {pages} pages"))?;

            // Pin the bounce buffer pages, if required.
            if !pin_pages.is_pinned(block.pfns()) {
                if !pin_pages.pin_pages(block.pfns()) {
                    anyhow::bail!("unable to pin bounce buffer pages");
                }
            }

            Some(PageAllocator::new(block, pages as usize).context("page allocator")?)
        } else {
            None
        };

        // Create the client. The client only supports mapping if pinning is
        // required.
        let dma_client = Arc::new(OpenhclDmaClient {
            inner: self.clone(),
            backing,
            params,
            bounce_pfns,
        });

        let dma_map: Option<Arc<dyn DmaMap>> = if self.pin_pages.is_some() {
            Some(dma_client.clone())
        } else {
            None
        };

        Ok(DmaClient::new(dma_client, dma_map))
    }
}

impl OpenhclDmaManager {
    /// Creates a new [`OpenhclDmaManager`] with the given ranges to use for the
    /// shared and private gpa pools.
    ///
    /// `pin_ranges` determines if ranges must be mapped and pinned before dma.
    pub fn new(
        shared_ranges: &[MemoryRange],
        private_ranges: &[MemoryRange],
        vtom: u64,
        pin_ranges: bool,
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

        let pin_pages = if pin_ranges {
            Some(PinPages::new().context("failed to create pin pages")?)
        } else {
            None
        };

        Ok(OpenhclDmaManager {
            inner: Arc::new(DmaManagerInner {
                shared_spawner: shared_pool.as_ref().map(|pool| pool.allocator_spawner()),
                private_spawner: private_pool.as_ref().map(|pool| pool.allocator_spawner()),
                lower_vtl: DmaManagerLowerVtl::new().context("failed to create lower vtl")?,
                pin_pages,
            }),
            shared_pool,
            private_pool,
        })
    }

    /// Creates a new DMA client with the given device name and lower VTL
    /// policy.
    pub fn new_client(&self, params: DmaClientParameters) -> anyhow::Result<DmaClient> {
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
    pub fn new_client(&self, params: DmaClientParameters) -> anyhow::Result<DmaClient> {
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

/// what we did to pages in a transaction
#[derive(Debug)]
enum DmaOperation<'a> {
    /// pages were already pinned/physically backed, original ranges
    PrePinned(PagedRange<'a>),
    /// pinned pages, must be unpinned, original ranges
    Pinned(PagedRange<'a>),
    /// allocated bounce buffers, original ranges
    Bounced {
        bounce: ScopedPages<'a>,
        bounce_pfns: Vec<u64>,
        original: PagedRange<'a>,
    },
}

enum CopyDirection {
    /// Copy from guest memory to bounce buffer
    ToBounce,
    /// Copy from bounce buffer to guest memory
    FromBounce,
}

fn copy_page_ranges(
    range: &PagedRange<'_>,
    guest_memory: &GuestMemory,
    bounce_range: &ScopedPages<'_>,
    direction: CopyDirection,
) -> anyhow::Result<()> {
    let mut index = 0;

    for range in range.ranges() {
        let range = range.context("invalid gpn")?;

        let mut len = range.len();
        let mut range_offset = 0;
        while len != 0 {
            let bounce_page = bounce_range.page_as_slice(index);
            let page_offset = ((range.start + range_offset) % PAGE_SIZE64) as usize;

            let copy_len = std::cmp::min(len as usize, PAGE_SIZE - page_offset);
            let bounce_page = &bounce_page[page_offset..page_offset + copy_len];

            match direction {
                CopyDirection::ToBounce => {
                    guest_memory
                        .read_to_atomic(range.start + range_offset, bounce_page)
                        .context("BUGBUG handle bounce copy error")?;
                }
                CopyDirection::FromBounce => {
                    guest_memory
                        .write_from_atomic(range.start + range_offset, bounce_page)
                        .context("BUGBUG handle bounce copy error")?;
                }
            }

            index += 1;
            len -= copy_len as u64;
            range_offset += copy_len as u64;
        }
    }

    Ok(())
}

#[derive(Debug)]
struct DmaTransaction<'a> {
    /// guest memory object to use to bounce in/out
    guest_memory: &'a GuestMemory,
    operation: DmaOperation<'a>,
    options: MapDmaOptions,
    pin_pages: &'a PinPages,
}

impl MappedDmaTransaction for DmaTransaction<'_> {
    fn pfns(&self) -> &[u64] {
        match &self.operation {
            DmaOperation::PrePinned(range) => range.gpns(),
            DmaOperation::Pinned(range) => range.gpns(),
            DmaOperation::Bounced { bounce_pfns, .. } => bounce_pfns,
        }
    }

    fn complete(&self) -> anyhow::Result<()> {
        match &self.operation {
            DmaOperation::PrePinned(_) => {}
            DmaOperation::Pinned(range) => {
                self.pin_pages.unpin_pages(range.gpns());
            }
            DmaOperation::Bounced {
                bounce,
                bounce_pfns: _,
                original,
            } => {
                if self.options.is_rx || self.options.always_bounce {
                    // copy from bounce buffer to guest memory
                    copy_page_ranges(
                        original,
                        self.guest_memory,
                        bounce,
                        CopyDirection::FromBounce,
                    )
                    .context("failed to copy from bounce buffer")?;
                }
            }
        }

        Ok(())
    }

    fn write_bounced(&self, buf: &[u8]) -> anyhow::Result<()> {
        match &self.operation {
            DmaOperation::Bounced { bounce, .. } => {
                bounce.write(buf);
                Ok(())
            }
            _ => anyhow::bail!("not a bounced transaction"),
        }
    }
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
            .ok_or(MapDmaError::NotEnoughBounceBufferSpace {
                range_bytes: range.len(),
            })?;

        // copy to bounced pages
        if options.is_tx {
            copy_page_ranges(&range, guest_memory, &bounce_pages, CopyDirection::ToBounce)
                .context("failed to copy to bounce buffer")
                .map_err(MapDmaError::Map)?;
        }

        Ok(DmaOperation::Bounced {
            bounce_pfns: bounce_pages.pfns().collect(),
            bounce: bounce_pages,
            original: range,
        })
    }

    async fn map_dma_ranges_inner<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Result<Box<dyn MappedDmaTransaction + 'a>, MapDmaError> {
        let pin_pages = self
            .inner
            .pin_pages
            .as_ref()
            .expect("map should not be called if pinning is not supported");

        // the transaction is either all bounced, or all pinned.
        let operation = if options.always_bounce {
            self.allocate_bounce_pages(guest_memory, range, options)
                .await?
        } else if pin_pages.is_pinned(range.gpns()) {
            DmaOperation::PrePinned(range)
        } else {
            if pin_pages.pin_pages(range.gpns()) {
                DmaOperation::Pinned(range)
            } else {
                self.allocate_bounce_pages(guest_memory, range, options)
                    .await?
            }
        };

        Ok(Box::new(DmaTransaction {
            guest_memory,
            operation,
            options,
            pin_pages,
        }))
    }
}

impl DmaAlloc for OpenhclDmaClient {
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

impl DmaMap for OpenhclDmaClient {
    fn map_dma_ranges<'a, 'b: 'a>(
        &'a self,
        guest_memory: &'a GuestMemory,
        range: PagedRange<'b>,
        options: MapDmaOptions,
    ) -> Pin<Box<dyn Future<Output = Result<Box<dyn MappedDmaTransaction + 'a>, MapDmaError>> + 'a>>
    {
        Box::pin(self.map_dma_ranges_inner(guest_memory, range, options))
    }

    fn unmap_dma_ranges(
        &self,
        transaction: Box<dyn MappedDmaTransaction + '_>,
    ) -> Result<(), MapDmaError> {
        transaction.complete().map_err(MapDmaError::Unmap)
    }
}

#[cfg(test)]
mod tests {
    use crate::copy_page_ranges;
    use crate::CopyDirection;
    use guestmem::ranges::PagedRange;
    use guestmem::GuestMemory;
    use guestmem::MemoryRead;
    use guestmem::MemoryWrite;
    use guestmem::PAGE_SIZE;
    use memory_range::MemoryRange;
    use page_pool_alloc::PagePool;
    use page_pool_alloc::TestMapper;
    use pal_async_test::async_test;
    use user_driver::page_allocator::PageAllocator;
    use user_driver::DmaAlloc;

    /// Create pools of 100 pages. Guest memory valid at page 0.
    fn create_pools() -> (GuestMemory, PagePool, PageAllocator) {
        let page_count = 100;
        let mem = GuestMemory::allocate(page_count * PAGE_SIZE);
        let pool = PagePool::new(
            &[MemoryRange::from_4k_gpn_range(0..page_count as u64)],
            TestMapper::new(page_count as u64).unwrap(),
        )
        .unwrap();
        let alloc = pool.allocator("test-pool".into()).unwrap();
        let block = alloc.allocate_dma_buffer(page_count * PAGE_SIZE).unwrap();
        let pages = PageAllocator::new(block, page_count).unwrap();

        (mem, pool, pages)
    }

    // Test copying to a bounce buffer with full pages
    #[async_test]
    async fn test_copy_to_bounce_full() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 10 pages
        let range = PagedRange::new(0, PAGE_SIZE * 10, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
        let bounce_range = pages.alloc_pages(10).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..PAGE_SIZE * 10)
            .map(|i| (i + (i / PAGE_SIZE)) as u8)
            .collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern.
        let mut bounce_buf = vec![0; PAGE_SIZE * 10];
        bounce_range.read(&mut bounce_buf);
        assert_eq!(buf, bounce_buf);
    }

    // Test copying to a bounce buffer with full pages, but the pages are not
    // all contiguous
    #[async_test]
    async fn test_copy_to_bounce_noncontiguous() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 10 pages
        let range =
            PagedRange::new(0, PAGE_SIZE * 10, &[0, 1, 2, 12, 24, 58, 32, 7, 8, 9]).unwrap();
        let bounce_range = pages.alloc_pages(10).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..PAGE_SIZE * 10)
            .map(|i| (i * i) as u8)
            .collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern.
        let mut bounce_buf = vec![0; PAGE_SIZE * 10];
        bounce_range.read(&mut bounce_buf);
        assert_eq!(buf, bounce_buf);
    }

    // Test copying from a bounce buffer with full pages
    #[async_test]
    async fn test_copy_from_bounce_full() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 10 pages
        let range = PagedRange::new(0, PAGE_SIZE * 10, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
        let bounce_range = pages.alloc_pages(10).await.unwrap();

        // Fill bounce buffer with a pattern
        let buf = (0..PAGE_SIZE * 10)
            .map(|i| (i + (i / PAGE_SIZE)) as u8)
            .collect::<Vec<_>>();
        bounce_range.write(&buf);

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::FromBounce).unwrap();

        // Check that the original range has the same pattern.
        let mut gm_buf = vec![0; PAGE_SIZE * 10];
        range.reader(&mem).read(&mut gm_buf).unwrap();
        assert_eq!(buf, gm_buf);
    }

    // Test copying to a bounce buffer with a partial starting page
    #[async_test]
    async fn test_copy_partial_page_start() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 3
        let offset = 123;
        let range = PagedRange::new(offset, PAGE_SIZE * 2 - 123, &[0, 3]).unwrap();
        let bounce_range = pages.alloc_pages(2).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..range.len()).map(|i| (i * i) as u8).collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern. Read the whole
        // pages, then remove the offset bytes.
        let mut bounce_buf = vec![0; PAGE_SIZE * 2];
        bounce_range.read(&mut bounce_buf);
        let bounce_data = &bounce_buf[offset..];
        assert_eq!(buf, bounce_data);
    }

    // Test copying to a bounce buffer with a partial starting page, contiguious range
    #[async_test]
    async fn test_copy_partial_page_start_contiguous() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 3
        let offset = 123;
        let range = PagedRange::new(offset, PAGE_SIZE * 2 - 123, &[0, 1]).unwrap();
        let bounce_range = pages.alloc_pages(2).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..range.len()).map(|i| (i * i) as u8).collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern. Read the whole
        // pages, then remove the offset bytes.
        let mut bounce_buf = vec![0; PAGE_SIZE * 2];
        bounce_range.read(&mut bounce_buf);
        let bounce_data = &bounce_buf[offset..];
        assert_eq!(buf, bounce_data);
    }

    // Test copying to a bounce buffer with a partial ending page
    #[async_test]
    async fn test_copy_partial_page_end() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 2 pages
        let range = PagedRange::new(0, PAGE_SIZE * 2 - 123, &[0, 3]).unwrap();
        let bounce_range = pages.alloc_pages(2).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..range.len()).map(|i| (i * i) as u8).collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern. Read the whole
        // pages, then remove the offset bytes.
        let mut bounce_buf = vec![0; PAGE_SIZE * 2];
        bounce_range.read(&mut bounce_buf);
        let bounce_data = &bounce_buf[..range.len()];
        assert_eq!(buf, bounce_data);
    }

    // Test copying to a bounce buffer with a partial start and end page
    #[async_test]
    async fn test_copy_partial_page_start_end() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 2 pages
        let offset = 123;
        let range = PagedRange::new(offset, PAGE_SIZE * 2 - 256, &[0, 3]).unwrap();
        let bounce_range = pages.alloc_pages(2).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..range.len()).map(|i| (i * i) as u8).collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern. Read the whole
        // pages, then remove the offset bytes.
        let mut bounce_buf = vec![0; PAGE_SIZE * 2];
        bounce_range.read(&mut bounce_buf);
        let bounce_data = &bounce_buf[offset..(offset + range.len())];
        assert_eq!(buf, bounce_data);
    }

    // Test copying to a bounce buffer with a single partial page
    #[async_test]
    async fn test_partial_single_page() {
        let (mem, _pool, pages) = create_pools();

        // Create transaction with 1 page
        let offset = 123;
        let range = PagedRange::new(offset, PAGE_SIZE - 2943, &[12]).unwrap();
        let bounce_range = pages.alloc_pages(1).await.unwrap();

        // Fill memory with a pattern, and see that the allocated pages have the
        // same pattern.
        let buf = (0..range.len()).map(|i| (i * i) as u8).collect::<Vec<_>>();
        range.writer(&mem).write(&buf).unwrap();

        copy_page_ranges(&range, &mem, &bounce_range, CopyDirection::ToBounce).unwrap();

        // Check that the bounce buffer has the same pattern. Read the whole
        // pages, then remove the offset bytes.
        let mut bounce_buf = vec![0; PAGE_SIZE];
        bounce_range.read(&mut bounce_buf);
        let bounce_data = &bounce_buf[offset..(offset + range.len())];
        assert_eq!(buf, bounce_data);
    }
}
