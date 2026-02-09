// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the region manager, which tracks regions and their mappings, as
//! well as partitions to map the regions into.

use crate::mapping_manager::Mappable;
use crate::mapping_manager::MappingManagerClient;
use crate::partition_mapper::PartitionMapper;
use futures::StreamExt;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use std::cmp::Ordering;
use thiserror::Error;
use vmcore::local_only::LocalOnly;

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
                .add_mapping(
                    range,
                    params.mappable.clone(),
                    params.file_offset,
                    params.writable,
                )
                .await;

            for partition in &mut self.inner.partitions {
                partition.notify_new_mapping(range).await;
            }
        }

        region.mappings.push(RegionMapping { params });
    }

    async fn remove_mappings(&mut self, id: RegionId, range_in_region: MemoryRange) {
        let index = self.region_index(id);
        let region = &mut self.regions[index];
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
        if let Some(region_range) = region.active_range() {
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
                .add_mapping(
                    range_within(region.params.range, mapping.params.range_in_region),
                    mapping.params.mappable.clone(),
                    mapping.params.file_offset,
                    mapping.params.writable && map_params.writable,
                )
                .await;
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

        let region_range = region.params.range;
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
    ) -> Result<RegionHandle, AddRegionError> {
        let params = RegionParams {
            name,
            range,
            priority,
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
    use crate::mapping_manager::MappingManager;
    use crate::region_manager::AddRegionError;
    use crate::region_manager::RegionId;
    use crate::region_manager::RegionParams;
    use memory_range::MemoryRange;
    use pal_async::async_test;
    use pal_async::task::Spawn;
    use std::ops::Range;

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

        let mm = MappingManager::new(spawn, 0x200000);
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
}
