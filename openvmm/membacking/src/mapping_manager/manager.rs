// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the mapping manager, which keeps track of the VA mappers and
//! their currently active mappings. It is responsible for invalidating mappings
//! in each VA range when they are torn down by the region manager.

use super::mappable::Mappable;
use super::object_cache::ObjectCache;
use super::object_cache::ObjectId;
use super::va_mapper::VaMapper;
use super::va_mapper::VaMapperError;
use crate::RemoteProcess;
use futures::StreamExt;
use futures::future::join_all;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use slab::Slab;
use std::sync::Arc;

/// The mapping manager.
#[derive(Debug, Inspect)]
pub struct MappingManager {
    #[inspect(
        flatten,
        with = "|x| inspect::send(&x.req_send, MappingRequest::Inspect)"
    )]
    client: MappingManagerClient,
}

impl MappingManager {
    /// Returns a new mapping manager that can map addresses up to `max_addr`.
    pub fn new(spawn: impl Spawn, max_addr: u64) -> Self {
        let (req_send, mut req_recv) = mesh::mpsc_channel();
        spawn
            .spawn("mapping_manager", {
                let mut task = MappingManagerTask::new();
                async move {
                    task.run(&mut req_recv).await;
                }
            })
            .detach();
        Self {
            client: MappingManagerClient {
                id: ObjectId::new(),
                req_send,
                max_addr,
            },
        }
    }

    /// Returns an object used to access the mapping manager, potentially from a
    /// remote process.
    pub fn client(&self) -> &MappingManagerClient {
        &self.client
    }
}

/// Provides access to the mapping manager.
#[derive(Debug, MeshPayload, Clone)]
pub struct MappingManagerClient {
    req_send: mesh::Sender<MappingRequest>,
    id: ObjectId,
    max_addr: u64,
}

static MAPPER_CACHE: ObjectCache<VaMapper> = ObjectCache::new();

impl MappingManagerClient {
    /// Returns a VA mapper for this guest memory.
    ///
    /// This will single instance the mapper, so this is safe to call multiple times.
    pub async fn new_mapper(&self) -> Result<Arc<VaMapper>, VaMapperError> {
        // Get the VA mapper from the mapper cache if possible to avoid keeping
        // multiple VA ranges for this memory per process.
        MAPPER_CACHE
            .get_or_insert_with(&self.id, async {
                VaMapper::new(self.req_send.clone(), self.max_addr, None, false).await
            })
            .await
    }

    /// Returns a VA mapper for this guest memory with private RAM mode enabled.
    ///
    /// In private RAM mode, guest RAM is backed by anonymous pages in the
    /// VaMapper's SparseMapping rather than shared file-backed memory. Device
    /// memory mappings still flow through the normal MappingManager path.
    ///
    /// Uses the mapper cache so that subsequent calls to [`Self::new_mapper`]
    /// (e.g., from [`GuestMemoryClient::guest_memory`]) return the same
    /// private mapper with its committed pages intact.
    pub async fn new_private_mapper(&self) -> Result<Arc<VaMapper>, VaMapperError> {
        MAPPER_CACHE
            .get_or_insert_with(&self.id, async {
                VaMapper::new(self.req_send.clone(), self.max_addr, None, true).await
            })
            .await
    }

    /// Returns a VA mapper for this guest memory, but map everything into the
    /// address space of `process`.
    ///
    /// Each call will allocate a new unique mapper.
    pub async fn new_remote_mapper(
        &self,
        process: RemoteProcess,
    ) -> Result<Arc<VaMapper>, VaMapperError> {
        Ok(Arc::new(
            VaMapper::new(self.req_send.clone(), self.max_addr, Some(process), false).await?,
        ))
    }

    /// Adds an active mapping.
    ///
    /// TODO: currently this will panic if the mapping overlaps an existing
    /// mapping. This needs to be fixed to allow this to overlap existing
    /// mappings, in which case the old ones will be split and replaced.
    pub async fn add_mapping(
        &self,
        range: MemoryRange,
        mappable: Mappable,
        file_offset: u64,
        writable: bool,
    ) {
        let params = MappingParams {
            range,
            mappable,
            file_offset,
            writable,
        };

        self.req_send
            .call(MappingRequest::AddMapping, params)
            .await
            .unwrap();
    }

    /// Removes all mappings in `range`.
    ///
    /// TODO: allow this to split existing mappings.
    pub async fn remove_mappings(&self, range: MemoryRange) {
        self.req_send
            .call(MappingRequest::RemoveMappings, range)
            .await
            .unwrap();
    }
}

/// A mapping request message.
#[derive(MeshPayload)]
pub enum MappingRequest {
    AddMapper(Rpc<mesh::Sender<MapperRequest>, MapperId>),
    RemoveMapper(MapperId),
    SendMappings(MapperId, MemoryRange),
    AddMapping(Rpc<MappingParams, ()>),
    RemoveMappings(Rpc<MemoryRange, ()>),
    Inspect(inspect::Deferred),
}

#[derive(InspectMut)]
struct MappingManagerTask {
    #[inspect(with = "inspect_mappings")]
    mappings: Vec<Mapping>,
    #[inspect(skip)]
    mappers: Mappers,
}

fn inspect_mappings(mappings: &Vec<Mapping>) -> impl '_ + Inspect {
    inspect::adhoc(move |req| {
        let mut resp = req.respond();
        for mapping in mappings {
            resp.field(
                &mapping.params.range.to_string(),
                inspect::adhoc(|req| {
                    req.respond()
                        .field("writable", mapping.params.writable)
                        .hex("file_offset", mapping.params.file_offset);
                }),
            );
        }
    })
}

struct Mapping {
    params: MappingParams,
    active_mappers: Vec<MapperId>,
}

/// The mapping parameters.
#[derive(MeshPayload, Clone)]
pub struct MappingParams {
    /// The memory range for the mapping.
    pub range: MemoryRange,
    /// The OS object to map.
    pub mappable: Mappable,
    /// The file offset into `mappable`.
    pub file_offset: u64,
    /// Whether to map the memory as writable.
    pub writable: bool,
}

struct Mappers {
    mappers: Slab<MapperComm>,
}

struct MapperComm {
    req_send: mesh::Sender<MapperRequest>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct MapperId(usize);

/// A request to a VA mapper.
#[derive(MeshPayload)]
pub enum MapperRequest {
    /// Map the specified mapping.
    Map(MappingParams),
    /// There is no mapping for the specified range, so release anything waiting
    /// on such a mapping to arrive.
    NoMapping(MemoryRange),
    /// Unmap the specified range and send a response when it's done.
    Unmap(Rpc<MemoryRange, ()>),
}

impl MappingManagerTask {
    fn new() -> Self {
        Self {
            mappers: Mappers {
                mappers: Slab::new(),
            },
            mappings: Vec::new(),
        }
    }

    async fn run(&mut self, req_recv: &mut mesh::Receiver<MappingRequest>) {
        while let Some(req) = req_recv.next().await {
            match req {
                MappingRequest::AddMapper(rpc) => rpc.handle_sync(|send| self.add_mapper(send)),
                MappingRequest::RemoveMapper(id) => {
                    self.remove_mapper(id);
                }
                MappingRequest::SendMappings(id, range) => {
                    self.send_mappings(id, range);
                }
                MappingRequest::AddMapping(rpc) => {
                    rpc.handle_sync(|params| self.add_mapping(params))
                }
                MappingRequest::RemoveMappings(rpc) => {
                    rpc.handle(async |range| self.remove_mappings(range).await)
                        .await
                }
                MappingRequest::Inspect(deferred) => deferred.inspect(&mut *self),
            }
        }
    }

    fn add_mapper(&mut self, req_send: mesh::Sender<MapperRequest>) -> MapperId {
        let id = self.mappers.mappers.insert(MapperComm { req_send });
        tracing::debug!(?id, "adding mapper");
        MapperId(id)
    }

    fn remove_mapper(&mut self, id: MapperId) {
        tracing::debug!(?id, "removing mapper");
        self.mappers.mappers.remove(id.0);
        for mapping in &mut self.mappings {
            mapping.active_mappers.retain(|m| m != &id);
        }
    }

    fn send_mappings(&mut self, id: MapperId, mut range: MemoryRange) {
        while !range.is_empty() {
            // Find the next mapping that overlaps range.
            let (this_end, params) = if let Some(mapping) = self
                .mappings
                .iter_mut()
                .filter(|mapping| mapping.params.range.overlaps(&range))
                .min_by_key(|mapping| mapping.params.range.start())
            {
                if mapping.params.range.start() <= range.start() {
                    if !mapping.active_mappers.contains(&id) {
                        mapping.active_mappers.push(id);
                    }
                    // The next mapping overlaps with the start of our range.
                    (
                        mapping.params.range.end().min(range.end()),
                        Some(mapping.params.clone()),
                    )
                } else {
                    // There's a gap before the next mapping.
                    (mapping.params.range.start(), None)
                }
            } else {
                // No matching mappings, consume the rest of the range.
                (range.end(), None)
            };
            let this_range = MemoryRange::new(range.start()..this_end);
            let req = if let Some(params) = params {
                tracing::debug!(range = %this_range, full_range = %params.range, "sending mapping for range");
                MapperRequest::Map(params)
            } else {
                tracing::debug!(range = %this_range, "no mapping for range");
                MapperRequest::NoMapping(this_range)
            };
            self.mappers.mappers[id.0].req_send.send(req);
            range = MemoryRange::new(this_end..range.end());
        }
    }

    fn add_mapping(&mut self, params: MappingParams) {
        tracing::debug!(range = %params.range, "adding mapping");

        assert!(!self.mappings.iter().any(|m| m.params.range == params.range));

        self.mappings.push(Mapping {
            params,
            active_mappers: Vec::new(),
        });
    }

    async fn remove_mappings(&mut self, range: MemoryRange) {
        let mut mappers = Vec::new();
        self.mappings.retain_mut(|mapping| {
            if !range.contains(&mapping.params.range) {
                assert!(
                    !range.overlaps(&mapping.params.range),
                    "no partial unmappings allowed"
                );
                return true;
            }
            tracing::debug!(range = %mapping.params.range, "removing mapping");
            mappers.append(&mut mapping.active_mappers);
            false
        });
        mappers.sort();
        mappers.dedup();
        self.mappers.invalidate(&mappers, range).await;
    }
}

impl Mappers {
    async fn invalidate(&self, ids: &[MapperId], range: MemoryRange) {
        tracing::debug!(mapper_count = ids.len(), %range, "sending invalidations");
        join_all(ids.iter().map(async |&MapperId(i)| {
            if let Err(err) = self.mappers[i]
                .req_send
                .call(MapperRequest::Unmap, range)
                .await
            {
                tracing::warn!(
                    error = &err as &dyn std::error::Error,
                    "mapper dropped invalidate request"
                );
            }
        }))
        .await;
    }
}
