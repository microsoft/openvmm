// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the region manager, which tracks regions and their mappings, as
//! well as partitions to map the regions into.

// UNSAFETY: Calling unsafe DmaTarget::map_dma with validated VA pointers.
#![expect(unsafe_code)]

use crate::mapping_manager::Mappable;
use crate::mapping_manager::MappingManagerClient;
use crate::mapping_manager::MappingParams;
use crate::mapping_manager::VaMapper;
use crate::partition_mapper::PartitionMapper;
use anyhow::Context as _;
use futures::StreamExt;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use std::cmp::Ordering;
use std::sync::Arc;
use thiserror::Error;
use vmcore::local_only::LocalOnly;

/// A consumer of IOMMU-granularity DMA mapping events.
///
/// Unlike [`PartitionMemoryMap`](virt::PartitionMemoryMap), which maps entire
/// regions by VA pointer for lazy SLAT resolution, this trait receives
/// individual sub-mapping events with the backing fd + offset, suitable for
/// explicit IOMMU programming (VFIO type1, iommufd, etc.).
///
/// DMA targets receive notifications for **all** active sub-mappings,
/// including device BAR memory (regions with `dma_target: false`). The
/// `dma_target` flag controls only whether a region is exposed via
/// `GuestMemorySharing` (for vhost-user); IOMMU consumers need the full
/// GPA→backing map to program identity mappings for all guest-visible memory.
///
/// Implementations must be `Send + Sync` because they are stored behind `Arc`
/// in the region manager task.
pub trait DmaTarget: Send + Sync {
    /// Program an IOMMU mapping for `range` to the backing described by
    /// `mappable` at `file_offset`.
    ///
    /// `host_va` is the host virtual address of the mapping, provided when
    /// the target was registered with `needs_va = true`. When `None`, the
    /// implementation should use `mappable` and `file_offset` directly
    /// (e.g., iommufd).
    ///
    /// # Safety
    /// When `host_va` is `Some`, the pointed-to memory must be backed and
    /// must not be unmapped for the duration of the resulting IOMMU mapping.
    /// The caller (the crate-internal `DmaMapper`) guarantees this by holding an
    /// [`Arc<VaMapper>`] and calling `ensure_mapped` before each invocation.
    unsafe fn map_dma(
        &self,
        range: MemoryRange,
        host_va: Option<*const u8>,
        mappable: &Mappable,
        file_offset: u64,
    ) -> anyhow::Result<()>;

    /// Remove IOMMU mappings within `range`.
    ///
    /// The region manager may call this with a range that covers multiple
    /// prior `map_dma` calls (e.g., unmapping an entire region at once even
    /// though individual sub-mappings were mapped separately). The range
    /// will always be aligned to mapping boundaries — it will not bisect
    /// any prior mapping. Gaps within the range (unmapped sub-ranges) are
    /// expected and must not cause errors.
    fn unmap_dma(&self, range: MemoryRange) -> anyhow::Result<()>;
}

/// Wraps a [`DmaTarget`] for use by the region manager.
///
/// Holds an optional [`VaMapper`] to ensure pages are faulted in before
/// `map_dma` (needed for VFIO type1's `pin_user_pages`).
struct DmaMapper {
    id: DmaMapperId,
    target: Arc<dyn DmaTarget>,
    va_mapper: Option<Arc<VaMapper>>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct DmaMapperId(u64);

impl DmaMapper {
    /// Map a sub-mapping into the IOMMU.
    async fn map_dma(
        &self,
        range: MemoryRange,
        mappable: &Mappable,
        file_offset: u64,
    ) -> anyhow::Result<()> {
        // Ensure the VaMapper has the backing pages faulted in and compute
        // the host VA for type1 backends.
        let host_va = if let Some(va_mapper) = &self.va_mapper {
            va_mapper
                .ensure_mapped(range)
                .await
                .context("VA range has no backing mapping")?;
            // SAFETY: range.start() is within the VA reservation (ensured by
            // ensure_mapped succeeding), so this produces a valid pointer
            // within the mapping.
            Some(unsafe { va_mapper.as_ptr().add(range.start() as usize).cast_const() })
        } else {
            None
        };
        // SAFETY: When host_va is Some, the VaMapper has been ensure_mapped
        // for this range. The VaMapper is held alive by this DmaMapper (via
        // Arc), so the VA reservation persists. The pages are backed because
        // ensure_mapped succeeded. The IOMMU mapping will be torn down
        // (via unmap_dma in disable_region or remove_dma_mapper) before the
        // VaMapper releases the VA range.
        unsafe { self.target.map_dma(range, host_va, mappable, file_offset) }
    }

    /// Unmap a range from the IOMMU.
    fn unmap_dma(&self, range: MemoryRange) {
        if let Err(e) = self.target.unmap_dma(range) {
            tracing::warn!(
                error = &*e as &dyn std::error::Error,
                %range,
                "DMA unmap failed"
            );
        }
    }
}

/// The region manager.
#[derive(Debug, Inspect)]
pub struct RegionManager {
    #[inspect(
        flatten,
        with = "|x| inspect::send(&x.req_send, RegionRequest::Inspect)"
    )]
    client: RegionManagerClient,
}

/// Provides access to the region manager.
#[derive(Debug, MeshPayload, Clone)]
pub struct RegionManagerClient {
    req_send: mesh::Sender<RegionRequest>,
}

struct Region {
    id: RegionId,
    map_params: Option<MapParams>,
    is_active: bool,
    params: RegionParams,
    mappings: Vec<RegionMapping>,
}

#[derive(Debug, MeshPayload)]
struct RegionParams {
    name: String,
    range: MemoryRange,
    priority: u8,
    /// Whether mappings in this region are DMA targets (guest RAM or
    /// similar shareable memory).
    dma_target: bool,
}

#[derive(Copy, Clone, Debug, MeshPayload, PartialEq, Eq, Inspect)]
pub struct MapParams {
    pub writable: bool,
    pub executable: bool,
    pub prefetch: bool,
}

impl Region {
    fn active_range(&self) -> Option<MemoryRange> {
        if self.is_active {
            Some(self.params.range)
        } else {
            None
        }
    }
}

/// The task object for the region manager.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct RegionId(u64);

#[derive(InspectMut)]
struct RegionManagerTask {
    #[inspect(with = "inspect_regions")]
    regions: Vec<Region>,
    #[inspect(skip)]
    next_region_id: u64,
    #[inspect(skip)]
    inner: RegionManagerTaskInner,
}

fn inspect_regions(regions: &Vec<Region>) -> impl '_ + Inspect {
    inspect::adhoc(move |req| {
        let mut resp = req.respond();
        for region in regions {
            resp.field(
                &format!("{}:{}", region.params.range, &region.params.name),
                inspect::adhoc(|req| {
                    req.respond()
                        .field("map_params", region.map_params)
                        .field("is_active", region.is_active)
                        .field("priority", region.params.priority)
                        .field(
                            "mappings",
                            inspect::adhoc(|req| {
                                inspect_mappings(req, region.params.range.start(), &region.mappings)
                            }),
                        );
                }),
            );
        }
    })
}

fn inspect_mappings(req: inspect::Request<'_>, region_start: u64, mappings: &[RegionMapping]) {
    let mut resp = req.respond();
    for mapping in mappings {
        let range = MemoryRange::new(
            region_start + mapping.params.range_in_region.start()
                ..region_start + mapping.params.range_in_region.end(),
        )
        .to_string();

        resp.field(
            &range,
            inspect::adhoc(|req| {
                req.respond()
                    .field("writable", mapping.params.writable)
                    .hex("file_offset", mapping.params.file_offset);
            }),
        );
    }
}

struct RegionManagerTaskInner {
    partitions: Vec<PartitionMapper>,
    dma_mappers: Vec<DmaMapper>,
    next_dma_mapper_id: u64,
    mapping_manager: MappingManagerClient,
}

#[derive(MeshPayload)]
enum RegionRequest {
    AddRegion(Rpc<RegionParams, Result<RegionId, AddRegionError>>),
    RemoveRegion(Rpc<RegionId, ()>),
    MapRegion(Rpc<(RegionId, MapParams), ()>),
    UnmapRegion(Rpc<RegionId, ()>),
    AddMapping(Rpc<(RegionId, RegionMappingParams), ()>),
    RemoveMappings(Rpc<(RegionId, MemoryRange), ()>),
    AddPartition(
        LocalOnly<Rpc<PartitionMapper, Result<(), crate::partition_mapper::PartitionMapperError>>>,
    ),
    AddDmaMapper(LocalOnly<Rpc<(Arc<dyn DmaTarget>, bool), anyhow::Result<DmaMapperId>>>),
    RemoveDmaMapper(LocalOnly<DmaMapperId>),
    Inspect(inspect::Deferred),
}

struct RegionMapping {
    params: RegionMappingParams,
}

#[derive(MeshPayload)]
struct RegionMappingParams {
    range_in_region: MemoryRange,
    mappable: Mappable,
    file_offset: u64,
    writable: bool,
}

fn range_within(outer: MemoryRange, inner: MemoryRange) -> MemoryRange {
    assert!(inner.end() <= outer.len());
    MemoryRange::new(outer.start() + inner.start()..outer.start() + inner.end())
}

#[derive(Debug, Error, MeshPayload)]
pub enum AddRegionError {
    #[error("memory region {new} overlaps with existing region {existing}")]
    OverlapError { existing: String, new: String },
}

impl RegionManagerTask {
    fn new(mapping_manager: MappingManagerClient) -> Self {
        Self {
            regions: Vec::new(),
            next_region_id: 1,
            inner: RegionManagerTaskInner {
                mapping_manager,
                partitions: Vec::new(),
                dma_mappers: Vec::new(),
                next_dma_mapper_id: 0,
            },
        }
    }

    async fn run(&mut self, req_recv: &mut mesh::Receiver<RegionRequest>) {
        while let Some(req) = req_recv.next().await {
            match req {
                RegionRequest::AddMapping(rpc) => {
                    rpc.handle(async |(id, params)| self.add_mapping(id, params).await)
                        .await
                }
                RegionRequest::RemoveMappings(rpc) => {
                    rpc.handle(async |(id, range)| self.remove_mappings(id, range).await)
                        .await
                }
                RegionRequest::AddPartition(LocalOnly(rpc)) => {
                    rpc.handle(async |partition| self.add_partition(partition).await)
                        .await
                }
                RegionRequest::AddDmaMapper(LocalOnly(rpc)) => {
                    let ((target, needs_va), rpc) = rpc.split();
                    let result = self.add_dma_mapper(target, needs_va).await;
                    rpc.complete(result);
                }
                RegionRequest::RemoveDmaMapper(LocalOnly(id)) => {
                    self.remove_dma_mapper(id);
                }
                RegionRequest::AddRegion(rpc) => rpc.handle_sync(|params| self.add_region(params)),
                RegionRequest::RemoveRegion(rpc) => {
                    rpc.handle(async |id| self.unmap_region(id, true).await)
                        .await
                }
                RegionRequest::MapRegion(rpc) => {
                    rpc.handle(async |(id, params)| self.map_region(id, params).await)
                        .await
                }
                RegionRequest::UnmapRegion(rpc) => {
                    rpc.handle(async |id| self.unmap_region(id, false).await)
                        .await
                }
                RegionRequest::Inspect(deferred) => {
                    deferred.inspect(&mut *self);
                }
            }
        }
    }

    async fn add_partition(
        &mut self,
        partition: PartitionMapper,
    ) -> Result<(), crate::partition_mapper::PartitionMapperError> {
        // Map existing regions. On failure, all regions will be unmapped by the
        // region mapper's drop impl, so don't worry about that.
        for region in &self.regions {
            if region.is_active {
                partition
                    .map_region(region.params.range, region.map_params.unwrap())
                    .await?;
            }
        }
        self.inner.partitions.push(partition);
        Ok(())
    }

    async fn add_dma_mapper(
        &mut self,
        target: Arc<dyn DmaTarget>,
        needs_va: bool,
    ) -> anyhow::Result<DmaMapperId> {
        // Create a VaMapper if the target needs host VAs for IOMMU programming.
        let va_mapper = if needs_va {
            Some(
                self.inner
                    .mapping_manager
                    .new_mapper()
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?,
            )
        } else {
            None
        };

        let id = DmaMapperId(self.inner.next_dma_mapper_id);
        self.inner.next_dma_mapper_id += 1;

        let mapper = DmaMapper {
            id,
            target,
            va_mapper,
        };

        // Replay existing active sub-mappings so the new IOMMU consumer
        // gets the current state.
        for region in &self.regions {
            if region.is_active {
                for mapping in &region.mappings {
                    let range = range_within(region.params.range, mapping.params.range_in_region);
                    mapper
                        .map_dma(range, &mapping.params.mappable, mapping.params.file_offset)
                        .await?;
                }
            }
        }

        self.inner.dma_mappers.push(mapper);
        Ok(id)
    }

    fn remove_dma_mapper(&mut self, id: DmaMapperId) {
        if let Some(pos) = self.inner.dma_mappers.iter().position(|m| m.id == id) {
            let mapper = &self.inner.dma_mappers[pos];
            // Unmap all active sub-mappings from this mapper before removing it.
            for region in &self.regions {
                if region.is_active {
                    for mapping in &region.mappings {
                        let range =
                            range_within(region.params.range, mapping.params.range_in_region);
                        mapper.unmap_dma(range);
                    }
                }
            }
            self.inner.dma_mappers.swap_remove(pos);
        }
    }

    fn region_index(&self, id: RegionId) -> usize {
        self.regions.iter().position(|r| r.id == id).unwrap()
    }

    fn add_region(&mut self, params: RegionParams) -> Result<RegionId, AddRegionError> {
        // Ensure that this fully overlaps everything at lower priority, and
        // everything at higher priority fully overlaps this.
        let range = params.range;
        for other_region in &self.regions {
            let other_range = other_region.params.range;
            if !range.overlaps(&other_range) {
                continue;
            };
            let ok = match params.priority.cmp(&other_region.params.priority) {
                Ordering::Less => other_range.contains(&range),
                Ordering::Equal => other_range == range,
                Ordering::Greater => range.contains(&other_range),
            };
            if !ok {
                return Err(AddRegionError::OverlapError {
                    existing: other_region.params.name.clone(),
                    new: params.name,
                });
            }
        }

        tracing::debug!(
            range = %params.range,
            name = params.name,
            priority = params.priority,
            "new region"
        );

        let id = RegionId(self.next_region_id);
        self.next_region_id += 1;
        self.regions.push(Region {
            id,
            map_params: None,
            is_active: false,
            params,
            mappings: Vec::new(),
        });
        Ok(id)
    }

    /// Enables the highest priority region in `range`. Panics if any regions in
    /// `range` are already enabled.
    async fn enable_best_region(&mut self, mut range: MemoryRange) {
        while !range.is_empty() {
            // Pick the highest priority region with the lowest startest address
            // in the range. Since lower priority ranges must be fully contained
            // in higher priority ones, we can make the chosen region without
            // overlapping with a higher priority region.
            if let Some(region) = self
                .regions
                .iter_mut()
                .filter_map(|region| {
                    region.map_params?;
                    if !range.contains(&region.params.range) {
                        assert!(
                            !range.overlaps(&region.params.range),
                            "no overlap invariant violated"
                        );
                        return None;
                    }
                    assert!(!region.is_active);
                    Some(region)
                })
                .min_by_key(|region| {
                    (
                        region.params.range.start(),
                        u8::MAX - region.params.priority,
                    )
                })
            {
                self.inner.enable_region(region).await;
                range = MemoryRange::new(region.params.range.end()..range.end());
            } else {
                range = MemoryRange::EMPTY;
            }
        }
    }

    async fn map_region(&mut self, id: RegionId, map_params: MapParams) {
        let index = self.region_index(id);
        let region = &mut self.regions[index];
        let range = region.params.range;
        let priority = region.params.priority;
        if region.map_params == Some(map_params) {
            return;
        }

        tracing::debug!(
            name = region.params.name,
            range = %region.params.range,
            writable = map_params.writable,
            "mapping region"
        );

        // Disable any overlapping active regions if they are lower priority. If
        // they are higher priority, stop now since the active mappings won't change.
        let mut enable = true;
        for (other_index, other_region) in self.regions.iter_mut().enumerate() {
            if !other_region.is_active || !other_region.params.range.overlaps(&range) {
                continue;
            }
            if other_region.params.priority > priority
                || (other_region.params.priority == priority && other_index < index)
            {
                enable = false;
            } else {
                assert!(enable);
                self.inner.disable_region(other_region).await;
            }
        }

        self.regions[index].map_params = Some(map_params);
        if enable {
            self.enable_best_region(range).await;
        }
    }

    async fn unmap_region(&mut self, id: RegionId, remove: bool) {
        let index = self.region_index(id);
        let region = &mut self.regions[index];
        tracing::debug!(
            name = region.params.name,
            range = %region.params.range,
            remove,
            "unmapping region"
        );

        let active_range = region.is_active.then_some(region.params.range);
        if active_range.is_some() {
            self.inner.disable_region(region).await;
        }

        if remove {
            self.regions.remove(index);
        } else {
            region.map_params = None;
        }
        if let Some(range) = active_range {
            self.enable_best_region(range).await;
        }
    }

    async fn add_mapping(&mut self, id: RegionId, params: RegionMappingParams) {
        let index = self.region_index(id);
        let region = &mut self.regions[index];

        // TODO: split and remove existing mappings, atomically. This is
        // technically required by virtiofs DAX support.
        assert!(
            !region
                .mappings
                .iter()
                .any(|m| m.params.range_in_region.overlaps(&params.range_in_region))
        );

        if let Some(region_range) = region.active_range() {
            let range = range_within(region_range, params.range_in_region);
            self.inner
                .mapping_manager
                .add_mapping(MappingParams {
                    range,
                    mappable: params.mappable.clone(),
                    file_offset: params.file_offset,
                    writable: params.writable,
                    dma_target: region.params.dma_target,
                })
                .await;

            for partition in &mut self.inner.partitions {
                partition.notify_new_mapping(range).await;
            }

            for dma_mapper in &self.inner.dma_mappers {
                if let Err(e) = dma_mapper
                    .map_dma(range, &params.mappable, params.file_offset)
                    .await
                {
                    tracing::warn!(
                        error = &*e as &dyn std::error::Error,
                        %range,
                        "DMA mapper failed to map new sub-mapping"
                    );
                }
            }
        }

        region.mappings.push(RegionMapping { params });
    }

    async fn remove_mappings(&mut self, id: RegionId, range_in_region: MemoryRange) {
        let index = self.region_index(id);
        let region = &mut self.regions[index];
        let active_range = region.active_range();

        // Collect absolute GPA ranges of mappings being removed (before
        // mutating the vec) so we can notify DMA mappers.
        let removed_ranges: Vec<MemoryRange> = if active_range.is_some() {
            let region_range = region.params.range;
            region
                .mappings
                .iter()
                .filter(|m| range_in_region.contains(&m.params.range_in_region))
                .map(|m| range_within(region_range, m.params.range_in_region))
                .collect()
        } else {
            Vec::new()
        };

        region.mappings.retain_mut(|mapping| {
            if !range_in_region.contains(&mapping.params.range_in_region) {
                assert!(
                    !range_in_region.overlaps(&mapping.params.range_in_region),
                    "no partial unmappings allowed"
                );
                return true;
            }
            false
        });
        if let Some(region_range) = active_range {
            // Unmap DMA mappers first — IOMMU entries must be removed before
            // the VA mappings are torn down (same ordering as disable_region).
            for &removed in &removed_ranges {
                for dma_mapper in &self.inner.dma_mappers {
                    dma_mapper.unmap_dma(removed);
                }
            }

            self.inner
                .mapping_manager
                .remove_mappings(range_within(region_range, range_in_region))
                .await;

            // Currently there is no need to tell the partitions about the
            // removed mappings; they will find out when the underlying VA is
            // invalidated by the kernel.
        }
    }
}

impl RegionManagerTaskInner {
    async fn enable_region(&mut self, region: &mut Region) {
        assert!(!region.is_active);
        let map_params = region.map_params.unwrap();

        tracing::debug!(
            name = region.params.name,
            range = %region.params.range,
            writable = map_params.writable,
            "enabling region"
        );

        // Add the mappings for the region.
        for mapping in &region.mappings {
            self.mapping_manager
                .add_mapping(MappingParams {
                    range: range_within(region.params.range, mapping.params.range_in_region),
                    mappable: mapping.params.mappable.clone(),
                    file_offset: mapping.params.file_offset,
                    writable: mapping.params.writable && map_params.writable,
                    dma_target: region.params.dma_target,
                })
                .await;
        }

        // Map sub-mappings into DMA mappers (per-sub-mapping granularity).
        for mapping in &region.mappings {
            let range = range_within(region.params.range, mapping.params.range_in_region);
            for dma_mapper in &self.dma_mappers {
                if let Err(e) = dma_mapper
                    .map_dma(range, &mapping.params.mappable, mapping.params.file_offset)
                    .await
                {
                    tracing::warn!(
                        error = &*e as &dyn std::error::Error,
                        %range,
                        "DMA mapper failed to map sub-mapping during region enable"
                    );
                }
            }
        }

        // Map the region into the partitions.
        for partition in &mut self.partitions {
            partition
                .map_region(region.params.range, map_params)
                .await
                .expect("cannot recover from failed mapping");
        }

        region.is_active = true;
    }

    async fn disable_region(&mut self, region: &mut Region) {
        assert!(region.is_active);

        tracing::debug!(
            name = region.params.name,
            range = %region.params.range,
            "disabling region"
        );

        // Unmap DMA mappers first — IOMMU entries must be removed before
        // the VA mappings are torn down (type1's pin_user_pages pins are
        // released by unmap_dma, and the underlying pages must still be
        // valid at that point).
        let region_range = region.params.range;
        for dma_mapper in &mut self.dma_mappers {
            dma_mapper.unmap_dma(region_range);
        }

        for partition in &mut self.partitions {
            partition.unmap_region(region_range);
        }
        self.mapping_manager.remove_mappings(region_range).await;
        region.is_active = false;
    }
}

impl RegionManager {
    /// Returns a new region manager that sends mappings to `mapping_manager`.
    pub fn new(spawn: impl Spawn, mapping_manager: MappingManagerClient) -> Self {
        let (req_send, mut req_recv) = mesh::mpsc_channel();
        spawn
            .spawn("region_manager", {
                let mut task = RegionManagerTask::new(mapping_manager);
                async move {
                    task.run(&mut req_recv).await;
                }
            })
            .detach();
        Self {
            client: RegionManagerClient { req_send },
        }
    }

    /// Gets access to the region manager.
    pub fn client(&self) -> &RegionManagerClient {
        &self.client
    }
}

impl RegionManagerClient {
    /// Adds a partition mapper.
    ///
    /// This may only be called in the same process as the region manager.
    pub async fn add_partition(
        &self,
        partition: PartitionMapper,
    ) -> Result<(), crate::partition_mapper::PartitionMapperError> {
        self.req_send
            .call(|x| RegionRequest::AddPartition(LocalOnly(x)), partition)
            .await
            .unwrap()
    }

    /// Creates a new, empty, unmapped region.
    ///
    /// Returns a handle that will remove the region on drop.
    pub async fn new_region(
        &self,
        name: String,
        range: MemoryRange,
        priority: u8,
        dma_target: bool,
    ) -> Result<RegionHandle, AddRegionError> {
        let params = RegionParams {
            name,
            range,
            priority,
            dma_target,
        };

        let id = self
            .req_send
            .call(RegionRequest::AddRegion, params)
            .await
            .unwrap()?;

        Ok(RegionHandle {
            id: Some(id),
            req_send: self.req_send.clone(),
        })
    }
}

/// Client for registering DMA mappers with the region manager.
///
/// This is the public-facing handle for IOMMU consumers (VFIO, iommufd)
/// to register themselves. It exposes only `add_dma_mapper`, hiding the
/// rest of the region manager API.
#[derive(Clone)]
pub struct DmaMapperClient {
    req_send: mesh::Sender<RegionRequest>,
}

impl DmaMapperClient {
    pub(crate) fn new(region_manager: &RegionManagerClient) -> Self {
        Self {
            req_send: region_manager.req_send.clone(),
        }
    }

    /// Register a DMA target to receive sub-mapping events.
    ///
    /// This may only be called in the same process as the region manager.
    ///
    /// If `needs_va` is `true`, the region manager will maintain a `VaMapper`
    /// and pass a host VA to [`DmaTarget::map_dma`] for each sub-mapping.
    /// Use this for backends that program the IOMMU via host VAs (VFIO type1).
    ///
    /// If `needs_va` is `false`, no `VaMapper` is created and `host_va` will
    /// be `None`. Use this for backends that map from the fd directly (iommufd).
    ///
    /// The replay loop maps all existing active sub-mappings into the new
    /// consumer. On failure, already-mapped entries are **not** rolled back;
    /// the caller must clean up by dropping the [`DmaTarget`] (e.g., closing
    /// the VFIO container fd).
    ///
    /// Returns a [`DmaMapperHandle`] that removes the mapper when dropped.
    pub async fn add_dma_mapper(
        &self,
        target: Arc<dyn DmaTarget>,
        needs_va: bool,
    ) -> anyhow::Result<DmaMapperHandle> {
        let id = self
            .req_send
            .call(
                |x| RegionRequest::AddDmaMapper(LocalOnly(x)),
                (target, needs_va),
            )
            .await
            .unwrap()?;
        Ok(DmaMapperHandle {
            id: Some(id),
            req_send: self.req_send.clone(),
        })
    }
}

/// Handle to a registered DMA mapper.
///
/// Removes the mapper from the region manager on drop, unmapping all
/// active IOMMU entries.
pub struct DmaMapperHandle {
    id: Option<DmaMapperId>,
    req_send: mesh::Sender<RegionRequest>,
}

impl Drop for DmaMapperHandle {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.req_send
                .send(RegionRequest::RemoveDmaMapper(LocalOnly(id)));
        }
    }
}

/// A handle to a region.
///
/// Removes the region on drop.
#[derive(Debug)]
#[must_use]
pub struct RegionHandle {
    id: Option<RegionId>,
    req_send: mesh::Sender<RegionRequest>,
}

impl RegionHandle {
    /// Maps this region to a guest address.
    pub async fn map(&self, params: MapParams) {
        self.req_send
            .call(RegionRequest::MapRegion, (self.id.unwrap(), params))
            .await
            .unwrap()
    }

    /// Unmaps this region.
    pub async fn unmap(&self) {
        let _ = self
            .req_send
            .call(RegionRequest::UnmapRegion, self.id.unwrap())
            .await;
    }

    /// Adds a mapping to the region.
    ///
    /// TODO: allow this to split+overwrite existing mappings.
    pub async fn add_mapping(
        &self,
        range_in_region: MemoryRange,
        mappable: Mappable,
        file_offset: u64,
        writable: bool,
    ) {
        let _ = self
            .req_send
            .call(
                RegionRequest::AddMapping,
                (
                    self.id.unwrap(),
                    RegionMappingParams {
                        range_in_region,
                        mappable,
                        file_offset,
                        writable,
                    },
                ),
            )
            .await;
    }

    /// Removes the mappings in `range` within this region.
    ///
    /// TODO: allow this to split mappings.
    pub async fn remove_mappings(&self, range: MemoryRange) {
        let _ = self
            .req_send
            .call(RegionRequest::RemoveMappings, (self.id.unwrap(), range))
            .await;
    }

    /// Tears the region down, waiting for all mappings to be unreferenced.
    pub async fn teardown(mut self) {
        let _ = self
            .req_send
            .call(RegionRequest::RemoveRegion, self.id.take().unwrap())
            .await;
    }
}

impl Drop for RegionHandle {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            let _recv = self.req_send.call(RegionRequest::RemoveRegion, id);
            // Don't wait for the response.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MapParams;
    use super::RegionManagerTask;
    use crate::mapping_manager::Mappable;
    use crate::mapping_manager::MappingManager;
    use crate::region_manager::AddRegionError;
    use crate::region_manager::DmaTarget;
    use crate::region_manager::RegionId;
    use crate::region_manager::RegionMappingParams;
    use crate::region_manager::RegionParams;
    use memory_range::MemoryRange;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use parking_lot::Mutex;
    use std::ops::Range;
    use std::sync::Arc;

    /// Records map/unmap calls for test assertions.
    #[derive(Default)]
    struct RecordingDmaTarget {
        events: Mutex<Vec<DmaEvent>>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum DmaEvent {
        Map(MemoryRange),
        Unmap(MemoryRange),
    }

    impl DmaTarget for RecordingDmaTarget {
        unsafe fn map_dma(
            &self,
            range: MemoryRange,
            _host_va: Option<*const u8>,
            _mappable: &Mappable,
            _file_offset: u64,
        ) -> anyhow::Result<()> {
            self.events.lock().push(DmaEvent::Map(range));
            Ok(())
        }

        fn unmap_dma(&self, range: MemoryRange) -> anyhow::Result<()> {
            self.events.lock().push(DmaEvent::Unmap(range));
            Ok(())
        }
    }

    impl RecordingDmaTarget {
        fn take_events(&self) -> Vec<DmaEvent> {
            std::mem::take(&mut self.events.lock())
        }
    }

    /// Create a dummy Mappable for tests (cross-platform).
    fn test_mappable() -> Mappable {
        sparse_mmap::alloc_shared_memory(0x10000, "test-dma")
            .unwrap()
            .into()
    }

    #[async_test]
    async fn test_region_overlap(spawn: impl Spawn) {
        struct TestTask(RegionManagerTask);
        impl TestTask {
            async fn add(
                &mut self,
                priority: u8,
                range: Range<u64>,
            ) -> Result<RegionId, AddRegionError> {
                let id = self.0.add_region(RegionParams {
                    priority,
                    name: priority.to_string(),
                    range: MemoryRange::new(range),
                    dma_target: false,
                })?;
                self.0
                    .map_region(
                        id,
                        MapParams {
                            executable: true,
                            writable: true,
                            prefetch: false,
                        },
                    )
                    .await;
                Ok(id)
            }

            async fn remove(&mut self, id: RegionId) {
                self.0.unmap_region(id, true).await;
            }
        }

        let mm = MappingManager::new(spawn, 0x200000, false, None);
        let mut task = TestTask(RegionManagerTask::new(mm.client().clone()));

        let high = task.add(1, 0x1000..0x3000).await.unwrap();

        task.add(0, 0x2000..0x4000).await.unwrap_err();

        let low = task.add(0, 0x1000..0x3000).await.unwrap();

        task.remove(high).await;

        task.add(1, 0x2000..0x4000).await.unwrap_err();
        task.add(1, 0x2000..0x3000).await.unwrap_err();

        let _high = task.add(1, 0..0x10000).await.unwrap();

        task.remove(low).await;

        task.add(0, 0..0x20000).await.unwrap_err();

        let _low = task.add(0, 0x1000..0x8000).await.unwrap();
    }

    /// Helper that wraps RegionManagerTask for DMA tests.
    struct DmaTestTask {
        task: RegionManagerTask,
        mappable: Mappable,
    }

    impl DmaTestTask {
        fn new(spawn: impl Spawn) -> Self {
            let mm = MappingManager::new(spawn, 0x200000, false, None);
            Self {
                task: RegionManagerTask::new(mm.client().clone()),
                mappable: test_mappable(),
            }
        }

        async fn add_region(&mut self, range: Range<u64>) -> RegionId {
            let id = self
                .task
                .add_region(RegionParams {
                    priority: 0,
                    name: format!("{range:x?}"),
                    range: MemoryRange::new(range),
                    dma_target: false,
                })
                .unwrap();
            self.task
                .map_region(
                    id,
                    MapParams {
                        executable: true,
                        writable: true,
                        prefetch: false,
                    },
                )
                .await;
            id
        }

        async fn add_mapping(&mut self, id: RegionId, range_in_region: Range<u64>) {
            self.task
                .add_mapping(
                    id,
                    RegionMappingParams {
                        range_in_region: MemoryRange::new(range_in_region),
                        mappable: self.mappable.clone(),
                        file_offset: 0,
                        writable: true,
                    },
                )
                .await;
        }
    }

    #[async_test]
    async fn test_dma_replay_on_registration(spawn: impl Spawn) {
        let mut t = DmaTestTask::new(&spawn);
        let r = t.add_region(0x0..0x10000).await;
        t.add_mapping(r, 0x0..0x4000).await;
        t.add_mapping(r, 0x8000..0xC000).await;

        // Register a DMA mapper — it should replay the two active mappings.
        let target = Arc::new(RecordingDmaTarget::default());
        let id = t.task.add_dma_mapper(target.clone(), false).await.unwrap();

        assert_eq!(
            target.take_events(),
            vec![
                DmaEvent::Map(MemoryRange::new(0x0..0x4000)),
                DmaEvent::Map(MemoryRange::new(0x8000..0xC000)),
            ]
        );

        // Clean up.
        t.task.remove_dma_mapper(id);
    }

    #[async_test]
    async fn test_dma_live_map_unmap(spawn: impl Spawn) {
        let mut t = DmaTestTask::new(&spawn);
        let r = t.add_region(0x0..0x10000).await;

        let target = Arc::new(RecordingDmaTarget::default());
        let _id = t.task.add_dma_mapper(target.clone(), false).await.unwrap();
        target.take_events(); // discard empty replay

        // Adding a mapping to an active region should notify the DMA mapper.
        t.add_mapping(r, 0x0..0x4000).await;
        assert_eq!(
            target.take_events(),
            vec![DmaEvent::Map(MemoryRange::new(0x0..0x4000))]
        );

        // Removing the mapping should unmap it.
        t.task
            .remove_mappings(r, MemoryRange::new(0x0..0x4000))
            .await;
        assert_eq!(
            target.take_events(),
            vec![DmaEvent::Unmap(MemoryRange::new(0x0..0x4000))]
        );
    }

    #[async_test]
    async fn test_dma_disable_region_unmaps(spawn: impl Spawn) {
        let mut t = DmaTestTask::new(&spawn);
        let r = t.add_region(0x0..0x10000).await;
        t.add_mapping(r, 0x0..0x4000).await;
        t.add_mapping(r, 0x8000..0xC000).await;

        let target = Arc::new(RecordingDmaTarget::default());
        let _id = t.task.add_dma_mapper(target.clone(), false).await.unwrap();
        target.take_events(); // discard replay

        // Disabling the region should unmap the entire region range.
        t.task.unmap_region(r, false).await;
        assert_eq!(
            target.take_events(),
            vec![DmaEvent::Unmap(MemoryRange::new(0x0..0x10000))]
        );
    }

    #[async_test]
    async fn test_dma_remove_mapper_unmaps_all(spawn: impl Spawn) {
        let mut t = DmaTestTask::new(&spawn);
        let r = t.add_region(0x0..0x10000).await;
        t.add_mapping(r, 0x0..0x4000).await;
        t.add_mapping(r, 0x8000..0xC000).await;

        let target = Arc::new(RecordingDmaTarget::default());
        let id = t.task.add_dma_mapper(target.clone(), false).await.unwrap();
        target.take_events(); // discard replay

        // Removing the mapper should unmap each active sub-mapping.
        t.task.remove_dma_mapper(id);
        assert_eq!(
            target.take_events(),
            vec![
                DmaEvent::Unmap(MemoryRange::new(0x0..0x4000)),
                DmaEvent::Unmap(MemoryRange::new(0x8000..0xC000)),
            ]
        );
    }

    #[async_test]
    async fn test_dma_inactive_region_no_notifications(spawn: impl Spawn) {
        let mut t = DmaTestTask::new(&spawn);
        let r = t.add_region(0x0..0x10000).await;
        t.add_mapping(r, 0x0..0x4000).await;

        // Disable the region before registering the mapper.
        t.task.unmap_region(r, false).await;

        let target = Arc::new(RecordingDmaTarget::default());
        let _id = t.task.add_dma_mapper(target.clone(), false).await.unwrap();

        // No replay for inactive regions.
        assert_eq!(target.take_events(), vec![]);

        // Adding a mapping while inactive should also not notify.
        t.add_mapping(r, 0x8000..0xC000).await;
        assert_eq!(target.take_events(), vec![]);
    }
}
