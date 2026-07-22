// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the mapping manager, which keeps track of the VA mappers and
//! their currently active mappings. It is responsible for invalidating mappings
//! in each VA range when they are torn down by the region manager.

use super::mappable::Mappable;
use super::object_cache::ObjectCache;
use super::object_cache::ObjectId;
use super::va_mapper::MapperRole;
use super::va_mapper::VaMapper;
use super::va_mapper::VaMapperError;
use crate::RemoteProcess;
use crate::region_manager::MappingType;
use futures::StreamExt;
use futures::future::join_all;
use guestmem::ProvideShareableRegions;
use guestmem::ShareableRegion;
use inspect::Inspect;
use inspect::InspectMut;
use memory_range::MemoryRange;
use mesh::MeshPayload;
use mesh::error::RemoteError;
use mesh::rpc::FailableRpc;
use mesh::rpc::Rpc;
use mesh::rpc::RpcSend;
use pal_async::task::Spawn;
use slab::Slab;
use std::sync::Arc;
use thiserror::Error;

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
    /// Returns a new mapping manager (mapping addresses up to `max_addr`) and
    /// its primary VA mapper.
    ///
    /// The primary mapper is created here, as part of construction, so it is
    /// necessarily the first mapper in the owning process and is captured by the
    /// shared cache: every later
    /// [`new_mapper`](MappingManagerClient::new_mapper) in this process returns
    /// it. It is the loader's write target and the partition's fault resolver,
    /// and the only mapper eligible for soft large pages. It is always eager,
    /// and starts empty — it receives mappings as regions are added.
    ///
    /// Private memory imposes a "single local eager mapper" restriction, but
    /// that is enforced dynamically by the manager task as mappings and mappers
    /// come and go (private RAM may be added later, e.g. via hotplug) rather
    /// than captured here at construction time.
    pub async fn new(
        spawn: impl Spawn,
        max_addr: u64,
        minimum_va_alignment: Option<usize>,
    ) -> Result<(Self, Arc<VaMapper>), VaMapperError> {
        let this = Self::new_bare(spawn, max_addr, minimum_va_alignment);
        // Create the primary mapper as part of construction. Being first, it is
        // the instance the shared cache hands to every later `new_mapper` in
        // this process.
        let primary = this
            .client()
            .get_or_create_mapper(true, MapperRole::Primary)
            .await?;
        Ok((this, primary))
    }

    /// Spawns the manager task and builds the client, without creating a primary
    /// mapper.
    fn new_bare(spawn: impl Spawn, max_addr: u64, minimum_va_alignment: Option<usize>) -> Self {
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
                minimum_va_alignment,
            },
        }
    }

    /// Test-only constructor that builds a manager with no primary mapper (an
    /// empty mapper cache), so unit tests can exercise secondary/lazy mapper
    /// mechanics directly. Production code must use [`new`](Self::new).
    #[cfg(test)]
    pub(crate) fn new_without_primary(
        spawn: impl Spawn,
        max_addr: u64,
        minimum_va_alignment: Option<usize>,
    ) -> Self {
        Self::new_bare(spawn, max_addr, minimum_va_alignment)
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
    minimum_va_alignment: Option<usize>,
}

static MAPPER_CACHE: ObjectCache<VaMapper> = ObjectCache::new();

impl MappingManagerClient {
    /// Returns a secondary VA mapper for this guest memory.
    ///
    /// A *secondary* mapper is any host-side view of guest memory other than the
    /// primary one that the loader writes through and the partition resolves
    /// faults against (created by [`MappingManager::new`](MappingManager::new)).
    /// Secondary mappers are additional, remote views: for example a mapper in
    /// the VP process for WHP VTL2 emulation, an out-of-process device that needs
    /// to touch guest memory, or a DMA target. They never own the memory, and
    /// soft large pages are not applied to them: in soft-large-page mode a
    /// secondary mapper only ever gets 4 KB pages.
    ///
    /// When `eager` is true, the mapper receives all existing mappings
    /// immediately and gets new ones pushed synchronously. File-backed
    /// page faults will fail (since mappings should already be
    /// established). This is the right choice for the VP process, where
    /// the hypervisor does not forward page faults back to the VMM.
    ///
    /// When `eager` is false, the mapper is lazy: mappings are populated
    /// on demand via page faults. This avoids the cost of pushing every
    /// mapping change to processes that rarely access the mapped regions
    /// (e.g., device-emulation processes with virtio-fs DAX).
    ///
    /// The mapper is single-instanced per process via a cache. If a lazy
    /// mapper was previously created and an eager one is now requested,
    /// it is upgraded in place. In the process that owns the
    /// [`GuestMemoryManager`](crate::MemoryManager), that cached instance is the
    /// primary mapper created by [`MappingManager::new`](MappingManager::new);
    /// in other processes (device/DMA workers) this creates a fresh secondary
    /// mapper.
    pub async fn new_mapper(&self, eager: bool) -> Result<Arc<VaMapper>, VaMapperError> {
        self.get_or_create_mapper(eager, MapperRole::Secondary)
            .await
    }

    async fn get_or_create_mapper(
        &self,
        eager: bool,
        role: MapperRole,
    ) -> Result<Arc<VaMapper>, VaMapperError> {
        let mapper = MAPPER_CACHE
            .get_or_insert_with(&self.id, async {
                VaMapper::new(
                    self.req_send.clone(),
                    self.max_addr,
                    None,
                    self.minimum_va_alignment,
                    eager,
                    role,
                )
                .await
            })
            .await?;

        // If we need eager but the cached mapper is lazy (created by an
        // earlier lazy call), upgrade it.
        if eager && !mapper.is_eager() {
            self.req_send
                .call(MappingRequest::UpgradeToEager, mapper.mapper_id())
                .await
                .map_err(VaMapperError::MemoryManagerGone)?
                .map_err(VaMapperError::Registration)?;
        }

        Ok(mapper)
    }

    /// Returns a VA mapper for this guest memory, but map everything into the
    /// address space of `process`.
    ///
    /// Each call will allocate a new unique mapper.
    ///
    /// If private memory is present, this fails at registration time because it
    /// would be a second mapper (private RAM requires a single mapper); a remote
    /// mapper is only rejected on that count, not for being remote.
    pub async fn new_remote_mapper(
        &self,
        process: RemoteProcess,
    ) -> Result<Arc<VaMapper>, VaMapperError> {
        Ok(Arc::new(
            VaMapper::new(
                self.req_send.clone(),
                self.max_addr,
                Some(process),
                self.minimum_va_alignment,
                true, // eager — remote mappers used for partition mappings
                // Secondary: this backs a partition from a remote process, but
                // the soft-large-page work (deferred protect, first-write 2 MB
                // populate, fault resolution) runs on the single primary mapper
                // in the owning process. This mapper shares the same section
                // pages, so it just maps them plain read-write 4 KB.
                MapperRole::Secondary,
            )
            .await?,
        ))
    }

    /// Adds an active mapping.
    ///
    /// The mapping is pushed eagerly to all existing VA mappers. Returns an
    /// error if any mapper fails to establish the mapping.
    ///
    /// TODO: currently this will panic if the mapping overlaps an existing
    /// mapping. This needs to be fixed to allow this to overlap existing
    /// mappings, in which case the old ones will be split and replaced.
    pub async fn add_mapping(&self, params: MappingParams) -> anyhow::Result<()> {
        self.req_send
            .call_failable(MappingRequest::AddMapping, params)
            .await?;
        Ok(())
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

/// Parameters for registering a new VA mapper.
#[derive(MeshPayload)]
pub struct AddMapperParams {
    /// Channel for sending mapping requests to the mapper task.
    pub send: mesh::Sender<MapperRequest>,
    /// Whether the mapper is eager (mappings pushed immediately and replayed
    /// on creation) or lazy (mappings populated on demand via page faults).
    pub eager: bool,
}

/// A mapping request message.
#[derive(MeshPayload)]
pub enum MappingRequest {
    /// Register a new VA mapper.
    AddMapper(FailableRpc<AddMapperParams, MapperId>),
    RemoveMapper(MapperId),
    /// Request that mappings covering the given range be sent to the specified
    /// mapper via fire-and-forget `MapLazy` messages. Used by lazy mappers
    /// to populate on demand.
    SendMappings(MapperId, MemoryRange),
    /// Upgrade a lazy mapper to eager: replay all existing mappings and
    /// mark it for future pushes.
    UpgradeToEager(FailableRpc<MapperId, ()>),
    AddMapping(FailableRpc<MappingParams, ()>),
    RemoveMappings(Rpc<MemoryRange, ()>),
    /// Returns all mappings that have [`MappingType::Ram`] type.
    GetDmaTargetMappings(Rpc<(), Vec<MappingParams>>),
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
                        .field("mapping_type", mapping.params.mapping_type)
                        .field("backed_by_fd", mapping.params.backing.mappable().is_some())
                        .hex("file_offset", mapping.params.backing.file_offset());
                }),
            );
        }
    })
}

struct Mapping {
    params: MappingParams,
    active_mappers: Vec<MapperId>,
}

/// How a guest memory mapping is backed by host memory.
#[derive(Debug, MeshPayload, Clone)]
pub enum MappingBacking {
    /// Backed by a mappable OS object (shared memory or file). The mapping
    /// manager mmaps `mappable` at `file_offset` into each VA mapper, so the
    /// same physical pages are shared across all mappers (and shareable with
    /// other processes via `GuestMemorySharing`).
    File {
        /// The OS object to map.
        mappable: Mappable,
        /// The file offset into `mappable`.
        file_offset: u64,
    },
    /// Backed by private anonymous memory committed directly by the mapping
    /// manager. There is no backing fd, so the memory cannot be shared with
    /// other processes or programmed into DMA targets that require an fd; it is
    /// exposed to DMA targets purely by host VA.
    ///
    /// Private memory is only valid with a single mapper: its anonymous storage
    /// lives in one mapper's address space, with no backing fd, so it cannot be
    /// shared to a second mapper. The mapping manager rejects a second mapper
    /// while private memory is present, and rejects adding private RAM while
    /// more than one mapper exists (see the `AddMapper` handler and
    /// [`MappingManagerTask::add_mapping`]). A lone remote mapper is fine — the
    /// restriction is on mapper *count*, not on locality.
    ///
    /// Unlike file-backed memory, the storage *is* the VA mapping: there is no
    /// fd holding the pages. Tearing the mapping down (via the region manager's
    /// `remove_mappings`) decommits and zeroes the pages, so a private region
    /// must not be transiently disabled and re-enabled — doing so would lose
    /// guest memory. The region manager asserts against this.
    Private,
}

impl MappingBacking {
    /// Returns the backing object, if this mapping is file-backed.
    pub fn mappable(&self) -> Option<&Mappable> {
        match self {
            MappingBacking::File { mappable, .. } => Some(mappable),
            MappingBacking::Private => None,
        }
    }

    /// Returns the offset within the backing object, or 0 if there is none.
    pub fn file_offset(&self) -> u64 {
        match self {
            MappingBacking::File { file_offset, .. } => *file_offset,
            MappingBacking::Private => 0,
        }
    }
}

/// Host-memory policy for a mapping, applied to the mapped VA range after the
/// backing is established. Independent of how the mapping is backed, so it
/// lives here rather than on [`MappingBacking`].
///
/// There is deliberately no `Default` impl: the correct policy is
/// context-sensitive (guest RAM wants THP enabled, device memory does not), so
/// an implicit fallback would silently do the wrong thing. Use [`MemoryPolicy::none`]
/// when no special policy is desired, or construct the struct explicitly.
#[derive(Debug, Copy, Clone, MeshPayload)]
pub struct MemoryPolicy {
    /// Host NUMA node to strictly bind the mapping to (Linux `mbind(MPOL_BIND)`,
    /// Windows `MemExtendedParameterNumaNode`). `None` means OS default.
    pub numa_node: Option<u32>,
    /// Whether the range is advised as Transparent Huge Page eligible (Linux
    /// `madvise(MADV_HUGEPAGE)`).
    pub transparent_hugepages: bool,
    /// Whether this range is populated eagerly at build time (prefetch). On
    /// Windows this selects the *eager* soft-large-page path: the range is
    /// mapped read-write and its large pages are built up front by the
    /// build-time populate, with its first-fault windows pre-marked attempted
    /// (rather than the lazy deferred-protect, fault-time path).
    pub prefetch: bool,
}

impl MemoryPolicy {
    /// Returns a policy that requests no special host-memory placement: no NUMA
    /// binding (OS default placement) and no Transparent Huge Page advice.
    ///
    /// This may be the right choice for mappings that are not guest RAM, such
    /// as device memory. Guest RAM should instead construct the policy
    /// explicitly so that THP eligibility is a deliberate decision.
    pub fn none() -> Self {
        Self {
            numa_node: None,
            transparent_hugepages: false,
            prefetch: false,
        }
    }
}

/// The mapping parameters.
#[derive(Debug, MeshPayload, Clone)]
pub struct MappingParams {
    /// The memory range for the mapping.
    pub range: MemoryRange,
    /// How the mapping is backed by host memory.
    pub backing: MappingBacking,
    /// Whether to map the memory as writable.
    pub writable: bool,
    /// The type of memory being mapped.
    ///
    /// [`MappingType::Ram`] mappings are exposed via
    /// [`GuestMemorySharing`](guestmem::GuestMemorySharing) so that external
    /// consumers (vhost-user backends, etc.) can share the backing memory.
    pub mapping_type: MappingType,
    /// Host-memory policy (NUMA node binding, THP eligibility).
    pub policy: MemoryPolicy,
}

/// Error from a failed VA mapping operation.
#[derive(Debug, Error)]
#[error("failed to map {range}")]
pub struct MappingError {
    /// The GPA range that failed to map.
    pub range: MemoryRange,
    /// The underlying OS error.
    #[source]
    pub error: std::io::Error,
}

impl MappingError {
    pub(crate) fn new(range: MemoryRange, error: std::io::Error) -> Self {
        Self { range, error }
    }
}

struct Mappers {
    mappers: Slab<MapperComm>,
}

struct MapperComm {
    req_send: mesh::Sender<MapperRequest>,
    eager: bool,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, MeshPayload)]
pub struct MapperId(pub(crate) usize);

/// A request to a VA mapper.
#[derive(Debug, MeshPayload)]
pub enum MapperRequest {
    /// Map the specified mapping and respond with success/failure.
    /// Used by the eager path (`add_mapping`).
    MapEager(Rpc<MappingParams, Result<(), RemoteError>>),
    /// Map the specified mapping (fire-and-forget).
    /// Used by the lazy path (`send_mappings`). The mapper task wakes
    /// any pending waiters after processing.
    MapLazy(MappingParams),
    /// There is no mapping for the specified range. The mapper task
    /// wakes waiters with a failure.
    NoMapping(MemoryRange),
    /// Unmap the specified range and send a response when it's done.
    Unmap(Rpc<MemoryRange, ()>),
    /// Mark this mapper as eager and respond when done. Sent after all
    /// replay `MapEager` Rpcs have completed, so the mapper task
    /// processes it after all replays.
    SetEager(Rpc<(), ()>),
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
                MappingRequest::AddMapper(rpc) => {
                    rpc.handle_failable(async |params: AddMapperParams| {
                        // Private memory is anonymous: its storage *is* a single
                        // mapper's address space, with no backing fd to share, so
                        // a second mapper would get its own incoherent copy.
                        // Private RAM therefore requires at most one mapper.
                        // Reject a new mapper if private memory is already
                        // present; the reverse direction (private RAM added
                        // later) is enforced in `add_mapping`.
                        if !self.mappers.mappers.is_empty() && self.has_private_mapping() {
                            return Err(MappingError::new(
                                MemoryRange::EMPTY,
                                std::io::Error::other(
                                    "cannot add a second mapper while private memory is present",
                                ),
                            ));
                        }
                        self.add_mapper(params.send, params.eager).await
                    })
                    .await
                }
                MappingRequest::RemoveMapper(id) => {
                    self.remove_mapper(id);
                }
                MappingRequest::SendMappings(id, range) => {
                    self.send_mappings(id, range);
                }
                MappingRequest::UpgradeToEager(rpc) => {
                    rpc.handle_failable(async |id| self.upgrade_to_eager(id).await)
                        .await
                }
                MappingRequest::AddMapping(rpc) => {
                    rpc.handle_failable(async |params| self.add_mapping(params).await)
                        .await
                }
                MappingRequest::RemoveMappings(rpc) => {
                    rpc.handle(async |range| self.remove_mappings(range).await)
                        .await
                }
                MappingRequest::GetDmaTargetMappings(rpc) => {
                    rpc.handle_sync(|()| self.get_dma_target_mappings())
                }
                MappingRequest::Inspect(deferred) => deferred.inspect(&mut *self),
            }
        }
    }

    async fn add_mapper(
        &mut self,
        req_send: mesh::Sender<MapperRequest>,
        eager: bool,
    ) -> Result<MapperId, MappingError> {
        let id = self.mappers.mappers.insert(MapperComm { req_send, eager });
        let mapper_id = MapperId(id);
        tracing::debug!(?id, eager, "adding mapper");

        if eager {
            // Replay all existing mappings to the new eager mapper.
            let mut failed = None;
            for mapping in &mut self.mappings {
                match self.mappers.mappers[id]
                    .req_send
                    .call(MapperRequest::MapEager, mapping.params.clone())
                    .await
                {
                    Ok(Ok(())) => {
                        mapping.active_mappers.push(mapper_id);
                    }
                    Ok(Err(e)) => {
                        failed = Some(MappingError::new(
                            mapping.params.range,
                            std::io::Error::other(e),
                        ));
                        break;
                    }
                    Err(_) => {
                        failed = Some(MappingError::new(
                            MemoryRange::EMPTY,
                            std::io::Error::other("mapper gone during replay"),
                        ));
                        break;
                    }
                }
            }
            if let Some(err) = failed {
                self.remove_mapper(mapper_id);
                return Err(err);
            }
        }

        Ok(mapper_id)
    }

    fn remove_mapper(&mut self, id: MapperId) {
        tracing::debug!(?id, "removing mapper");
        self.mappers.mappers.remove(id.0);
        for mapping in &mut self.mappings {
            mapping.active_mappers.retain(|m| m != &id);
        }
    }

    /// Upgrade a mapper from lazy to eager: mark it eager and replay all
    /// existing mappings. On failure, the mapper stays lazy.
    async fn upgrade_to_eager(&mut self, id: MapperId) -> Result<(), MappingError> {
        let mapper = &mut self.mappers.mappers[id.0];
        if mapper.eager {
            return Ok(()); // already eager
        }
        // Mark eager on the manager side first so new add_mapping calls
        // will push to this mapper. The mapper task itself isn't marked
        // eager until SetEager is processed (after all replays succeed).
        //
        // This is safe because the manager task processes requests
        // sequentially — no concurrent add_mapping can run while this
        // method is executing. If the upgrade fails, we roll back
        // `mapper.eager` to `false` before returning.
        mapper.eager = true;
        tracing::debug!(?id, "upgrading mapper to eager");

        let mut failed = None;
        for mapping in &mut self.mappings {
            // Skip mappings already established by lazy resolution.
            if mapping.active_mappers.contains(&id) {
                continue;
            }
            match self.mappers.mappers[id.0]
                .req_send
                .call(MapperRequest::MapEager, mapping.params.clone())
                .await
            {
                Ok(Ok(())) => {
                    mapping.active_mappers.push(id);
                }
                Ok(Err(e)) => {
                    failed = Some(MappingError::new(
                        mapping.params.range,
                        std::io::Error::other(e),
                    ));
                    break;
                }
                Err(_) => {
                    failed = Some(MappingError::new(
                        MemoryRange::EMPTY,
                        std::io::Error::other("mapper gone during eager upgrade"),
                    ));
                    break;
                }
            }
        }

        if let Some(err) = failed {
            // Roll back: unmark eager so future add_mapping calls don't
            // push to this mapper. Keep successfully-replayed entries in
            // active_mappers so the mapper gets Unmap when those mappings
            // are removed (the VA space already has them mapped).
            self.mappers.mappers[id.0].eager = false;
            return Err(err);
        }

        // Tell the mapper task to mark itself eager. Since the mapper task
        // processes messages sequentially, this runs after all the MapEager
        // replays above.
        self.mappers.mappers[id.0]
            .req_send
            .call(MapperRequest::SetEager, ())
            .await
            .ok();

        Ok(())
    }

    /// Handle a lazy mapper's on-demand request for mappings covering `range`.
    ///
    /// Finds all mappings that overlap the requested range and sends them
    /// to the mapper via fire-and-forget `MapLazy` messages. Gaps send
    /// `NoMapping` so the mapper task can wake waiters with failure.
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
                tracing::debug!(range = %this_range, full_range = %params.range, "sending lazy mapping");
                MapperRequest::MapLazy(params)
            } else {
                tracing::debug!(range = %this_range, "no mapping for range");
                MapperRequest::NoMapping(this_range)
            };
            self.mappers.mappers[id.0].req_send.send(req);
            range = MemoryRange::new(this_end..range.end());
        }
    }

    async fn add_mapping(&mut self, params: MappingParams) -> anyhow::Result<()> {
        tracing::debug!(range = %params.range, "adding mapping");

        // Private memory is anonymous and lives in a single mapper's address
        // space, so it requires at most one mapper. Reject adding private RAM
        // (e.g. via hotplug) while more than one mapper is present.
        if matches!(params.backing, MappingBacking::Private) && self.mappers.mappers.len() > 1 {
            anyhow::bail!("cannot add private memory while multiple mappers are present");
        }

        assert!(!self.mappings.iter().any(|m| m.params.range == params.range));

        // Push to eager mappers only. Lazy mappers will request on demand.
        let mut active_mappers = Vec::new();
        for (i, mapper) in self.mappers.mappers.iter() {
            if !mapper.eager {
                continue;
            }
            let id = MapperId(i);
            match mapper
                .req_send
                .call(MapperRequest::MapEager, params.clone())
                .await
            {
                Ok(Ok(())) => {
                    active_mappers.push(id);
                }
                Ok(Err(e)) => {
                    // Unmap from mappers that already succeeded before returning
                    // the error.
                    for &rollback_id in &active_mappers {
                        if let Err(err) = self.mappers.mappers[rollback_id.0]
                            .req_send
                            .call(MapperRequest::Unmap, params.range)
                            .await
                        {
                            tracing::warn!(
                                error = &err as &dyn std::error::Error,
                                "mapper dropped unmap during rollback"
                            );
                        }
                    }
                    return Err(e.into());
                }
                Err(_) => {
                    // Mapper gone, skip. VaMapper::drop sends RemoveMapper
                    // which cleans up the stale entry.
                    tracing::debug!(?id, "mapper gone during add_mapping");
                }
            }
        }

        self.mappings.push(Mapping {
            params,
            active_mappers,
        });
        Ok(())
    }

    /// Returns true if any current mapping is backed by private memory.
    fn has_private_mapping(&self) -> bool {
        self.mappings
            .iter()
            .any(|m| matches!(m.params.backing, MappingBacking::Private))
    }

    fn get_dma_target_mappings(&self) -> Vec<MappingParams> {
        self.mappings
            .iter()
            // Only file-backed RAM can be shared with other processes;
            // private/anonymous RAM has no fd to hand out.
            .filter(|m| {
                m.params.mapping_type == MappingType::Ram && m.params.backing.mappable().is_some()
            })
            .map(|m| m.params.clone())
            .collect()
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

/// Implements [`ProvideShareableRegions`] by querying the
/// [`MappingManager`] for DMA-target mappings. Used by `VaMapper`'s
/// `sharing()` implementation.
pub(crate) struct DmaRegionProvider {
    pub req_send: mesh::Sender<MappingRequest>,
}

impl ProvideShareableRegions for DmaRegionProvider {
    async fn get_regions(&self) -> Result<Vec<ShareableRegion>, guestmem::ShareableRegionError> {
        let mappings = self
            .req_send
            .call(MappingRequest::GetDmaTargetMappings, ())
            .await?;

        Ok(mappings
            .into_iter()
            .filter_map(|m| {
                let mappable = m.backing.mappable()?;
                Some(ShareableRegion {
                    guest_address: m.range.start(),
                    size: m.range.len(),
                    file: mappable.inner_arc(),
                    file_offset: m.backing.file_offset(),
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region_manager::MappingType;
    use guestmem::GuestMemoryAccess;
    use guestmem::ProvideShareableRegions;
    use memory_range::MemoryRange;

    #[pal_async::async_test]
    async fn test_dma_target_regions_returned(spawn: impl Spawn) {
        let mm = MappingManager::new_without_primary(&spawn, 0x200000, None);
        let client = mm.client().clone();

        let ram: Mappable = sparse_mmap::alloc_shared_memory(0x100000, "test-ram")
            .unwrap()
            .into();
        let device: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test-dev")
            .unwrap()
            .into();

        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0..0x100000),
                backing: MappingBacking::File {
                    mappable: ram,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Ram,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0x100000..0x101000),
                backing: MappingBacking::File {
                    mappable: device,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Device,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        let provider = DmaRegionProvider {
            req_send: client.req_send.clone(),
        };
        let regions = provider.get_regions().await.unwrap();

        // Only the DMA-target mapping should appear.
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].guest_address, 0);
        assert_eq!(regions[0].size, 0x100000);
        assert_eq!(regions[0].file_offset, 0);
    }

    #[pal_async::async_test]
    async fn test_no_dma_targets_returns_empty(spawn: impl Spawn) {
        let mm = MappingManager::new_without_primary(&spawn, 0x100000, None);
        let client = mm.client().clone();

        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test")
            .unwrap()
            .into();

        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0..0x1000),
                backing: MappingBacking::File {
                    mappable,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Device,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        let provider = DmaRegionProvider {
            req_send: client.req_send.clone(),
        };
        let regions = provider.get_regions().await.unwrap();
        assert!(regions.is_empty());
    }

    /// Helper: create a MappingManagerTask and add a mapping.
    async fn task_with_mapping() -> (MappingManagerTask, MappingParams) {
        let mut task = MappingManagerTask::new();
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x10000, "test")
            .unwrap()
            .into();
        let params = MappingParams {
            range: MemoryRange::new(0..0x10000),
            backing: MappingBacking::File {
                mappable,
                file_offset: 0,
            },
            writable: true,
            mapping_type: MappingType::Ram,
            policy: MemoryPolicy::none(),
        };
        task.add_mapping(params.clone()).await.unwrap();
        (task, params)
    }

    /// Helper: add a mapper to a task and collect the MapEager/MapLazy messages
    /// it receives.
    async fn add_mapper_and_drain(
        task: &mut MappingManagerTask,
        eager: bool,
    ) -> (MapperId, Vec<MapperRequest>) {
        let (send, mut recv) = mesh::channel();
        let id = task.add_mapper(send, eager).await.unwrap();
        // Drain all pending messages.
        let mut msgs = Vec::new();
        while let Ok(msg) = recv.try_recv() {
            msgs.push(msg);
        }
        (id, msgs)
    }

    #[pal_async::async_test]
    async fn test_eager_mapper_gets_replay(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        let (send, mut recv) = mesh::channel();
        // add_mapper for eager blocks on the MapEager Rpc, so drive both
        // concurrently.
        let (id, _) = futures::join!(task.add_mapper(send, true), async {
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => {
                    let (params, rpc) = rpc.split();
                    assert_eq!(params.range, MemoryRange::new(0..0x10000));
                    rpc.complete(Ok(()));
                }
                other => panic!("expected MapEager, got {other:?}"),
            }
        });
        let _ = id;
    }

    #[pal_async::async_test]
    async fn test_lazy_mapper_no_replay(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        let (_id, msgs) = add_mapper_and_drain(&mut task, false).await;

        // Lazy mapper should receive no messages on creation.
        assert!(msgs.is_empty(), "lazy mapper should not get replay");
    }

    #[pal_async::async_test]
    async fn test_add_mapping_pushes_only_to_eager(_spawn: impl Spawn) {
        let mut task = MappingManagerTask::new();

        // Add one eager and one lazy mapper.
        let (eager_send, mut eager_recv) = mesh::channel();
        let _eager_id = task.add_mapper(eager_send, true).await.unwrap();

        let (lazy_send, mut lazy_recv) = mesh::channel();
        let _lazy_id = task.add_mapper(lazy_send, false).await.unwrap();

        // Add a mapping.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test")
            .unwrap()
            .into();
        let params = MappingParams {
            range: MemoryRange::new(0..0x1000),
            backing: MappingBacking::File {
                mappable,
                file_offset: 0,
            },
            writable: true,
            mapping_type: MappingType::Device,
            policy: MemoryPolicy::none(),
        };

        // The eager mapper needs to respond to the MapEager Rpc.
        let add_future = task.add_mapping(params);
        // Drive the add_mapping by responding to the eager mapper's Rpc.
        let (add_result, _) = futures::join!(add_future, async {
            let msg = eager_recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                other => panic!("expected MapEager, got {other:?}"),
            }
        });
        add_result.unwrap();

        // Lazy mapper should have received nothing.
        assert!(
            lazy_recv.try_recv().is_err(),
            "lazy mapper should not be notified on add_mapping"
        );
    }

    #[pal_async::async_test]
    async fn test_upgrade_to_eager_replays(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        // Add a lazy mapper — no replay.
        let (send, mut recv) = mesh::channel();
        let id = task.add_mapper(send, false).await.unwrap();
        assert!(
            recv.try_recv().is_err(),
            "lazy mapper should not get replay"
        );

        // Upgrade to eager — should replay existing mappings.
        let upgrade_future = task.upgrade_to_eager(id);
        let (result, _) = futures::join!(upgrade_future, async {
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => {
                    let (params, rpc) = rpc.split();
                    assert_eq!(params.range, MemoryRange::new(0..0x10000));
                    rpc.complete(Ok(()));
                }
                other => panic!("expected MapEager during upgrade, got {other:?}"),
            }
            // Respond to the SetEager RPC.
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::SetEager(rpc) => rpc.complete(()),
                other => panic!("expected SetEager, got {other:?}"),
            }
        });
        result.unwrap();

        // Verify the mapper is now marked eager.
        assert!(task.mappers.mappers[id.0].eager);
    }

    #[pal_async::async_test]
    async fn test_upgrade_already_eager_is_noop(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        // Add an eager mapper (responds to replay).
        let (send, mut recv) = mesh::channel();
        let upgrade_future = task.add_mapper(send, true);
        let (id, _) = futures::join!(upgrade_future, async {
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                other => panic!("expected MapEager, got {other:?}"),
            }
        });
        let id = id.unwrap();

        // Upgrade again — should be a no-op, no messages sent.
        task.upgrade_to_eager(id).await.unwrap();
        assert!(
            recv.try_recv().is_err(),
            "upgrade of already-eager mapper should send nothing"
        );
    }

    #[pal_async::async_test]
    async fn test_after_upgrade_new_mappings_are_pushed(_spawn: impl Spawn) {
        let mut task = MappingManagerTask::new();

        // Add a lazy mapper.
        let (send, mut recv) = mesh::channel();
        let id = task.add_mapper(send, false).await.unwrap();

        // Upgrade to eager (no existing mappings, so no replay, but SetEager
        // is still sent as an RPC).
        let upgrade_future = task.upgrade_to_eager(id);
        let (result, _) = futures::join!(upgrade_future, async {
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::SetEager(rpc) => rpc.complete(()),
                other => panic!("expected SetEager, got {other:?}"),
            }
        });
        result.unwrap();

        // Now add a mapping — should be pushed to the upgraded mapper.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test")
            .unwrap()
            .into();
        let params = MappingParams {
            range: MemoryRange::new(0..0x1000),
            backing: MappingBacking::File {
                mappable,
                file_offset: 0,
            },
            writable: true,
            mapping_type: MappingType::Device,
            policy: MemoryPolicy::none(),
        };

        let add_future = task.add_mapping(params);
        let (result, _) = futures::join!(add_future, async {
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                other => panic!("expected MapEager after upgrade, got {other:?}"),
            }
        });
        result.unwrap();
    }

    #[pal_async::async_test]
    async fn test_send_mappings_for_lazy(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        // Add a lazy mapper.
        let (send, mut recv) = mesh::channel();
        let id = task.add_mapper(send, false).await.unwrap();

        // Request mappings for the range (simulates page fault path).
        task.send_mappings(id, MemoryRange::new(0..0x10000));

        // Should receive a MapLazy.
        let msg = recv.recv().await.unwrap();
        match msg {
            MapperRequest::MapLazy(params) => {
                assert_eq!(params.range, MemoryRange::new(0..0x10000));
            }
            other => panic!("expected MapLazy, got {other:?}"),
        }
    }

    #[pal_async::async_test]
    async fn test_send_mappings_gap_sends_no_mapping(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        let (send, mut recv) = mesh::channel();
        let id = task.add_mapper(send, false).await.unwrap();

        // Request a range that is partially unmapped (0x10000..0x20000 has no mapping).
        task.send_mappings(id, MemoryRange::new(0x10000..0x20000));

        let msg = recv.recv().await.unwrap();
        match msg {
            MapperRequest::NoMapping(range) => {
                assert_eq!(range, MemoryRange::new(0x10000..0x20000));
            }
            other => panic!("expected NoMapping, got {other:?}"),
        }
    }

    #[pal_async::async_test]
    async fn test_remove_mapping_invalidates_both_eager_and_lazy(_spawn: impl Spawn) {
        let (mut task, _params) = task_with_mapping().await;

        // Add eager mapper (responds to replay).
        let (eager_send, mut eager_recv) = mesh::channel();
        let add_future = task.add_mapper(eager_send, true);
        let (_eager_id, _) = futures::join!(add_future, async {
            let msg = eager_recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                other => panic!("expected MapEager, got {other:?}"),
            }
        });

        // Add lazy mapper and fault in the mapping.
        let (lazy_send, mut lazy_recv) = mesh::channel();
        let lazy_id = task.add_mapper(lazy_send, false).await.unwrap();
        task.send_mappings(lazy_id, MemoryRange::new(0..0x10000));
        // Consume the MapLazy.
        let _ = lazy_recv.recv().await.unwrap();

        // Remove the mapping — both should get Unmap.
        let remove_future = task.remove_mappings(MemoryRange::new(0..0x10000));
        let ((), _, _) = futures::join!(
            remove_future,
            async {
                let msg = eager_recv.recv().await.unwrap();
                match msg {
                    MapperRequest::Unmap(rpc) => {
                        let (range, rpc) = rpc.split();
                        assert_eq!(range, MemoryRange::new(0..0x10000));
                        rpc.complete(());
                    }
                    other => panic!("expected Unmap for eager, got {other:?}"),
                }
            },
            async {
                let msg = lazy_recv.recv().await.unwrap();
                match msg {
                    MapperRequest::Unmap(rpc) => {
                        let (range, rpc) = rpc.split();
                        assert_eq!(range, MemoryRange::new(0..0x10000));
                        rpc.complete(());
                    }
                    other => panic!("expected Unmap for lazy, got {other:?}"),
                }
            }
        );
    }

    /// Helper: create a task with two mappings for rollback tests.
    async fn task_with_two_mappings() -> MappingManagerTask {
        let mut task = MappingManagerTask::new();
        for (start, end) in [(0u64, 0x10000u64), (0x10000, 0x20000)] {
            let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x10000, "test")
                .unwrap()
                .into();
            task.add_mapping(MappingParams {
                range: MemoryRange::new(start..end),
                backing: MappingBacking::File {
                    mappable,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Ram,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();
        }
        task
    }

    #[pal_async::async_test]
    async fn test_add_eager_mapper_rollback_on_replay_failure(_spawn: impl Spawn) {
        let mut task = task_with_two_mappings().await;

        // Create a mapper that succeeds on the first mapping but fails
        // on the second.
        let (send, mut recv) = mesh::channel();
        let add_future = task.add_mapper(send, true);
        let (result, _) = futures::join!(add_future, async {
            // First MapEager: succeed.
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                other => panic!("expected MapEager #1, got {other:?}"),
            }
            // Second MapEager: fail.
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => {
                    rpc.complete(Err(RemoteError::new(std::io::Error::other(
                        "simulated failure",
                    ))));
                }
                other => panic!("expected MapEager #2, got {other:?}"),
            }
        });

        // add_mapper should return an error.
        assert!(result.is_err());

        // The mapper should have been removed from the slab.
        assert_eq!(task.mappers.mappers.len(), 0);

        // The first mapping's active_mappers should have been cleaned up.
        for mapping in &task.mappings {
            assert!(
                mapping.active_mappers.is_empty(),
                "active_mappers should be empty after rollback, got {:?} for {}",
                mapping.active_mappers,
                mapping.params.range
            );
        }

        // Behavioral check: adding another eager mapper should succeed and
        // replay both mappings cleanly (no stale state from failed mapper).
        let (send2, mut recv2) = mesh::channel();
        let add_future2 = task.add_mapper(send2, true);
        let (result2, _) = futures::join!(add_future2, async {
            for _ in 0..2 {
                let msg = recv2.recv().await.unwrap();
                match msg {
                    MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                    other => panic!("expected MapEager during second add, got {other:?}"),
                }
            }
        });
        assert!(result2.is_ok());
    }

    #[pal_async::async_test]
    async fn test_add_mapping_rollback_on_eager_failure(_spawn: impl Spawn) {
        let mut task = MappingManagerTask::new();

        // Add two eager mappers.
        let (send1, mut recv1) = mesh::channel();
        let _id1 = task.add_mapper(send1, true).await.unwrap();

        let (send2, mut recv2) = mesh::channel();
        let _id2 = task.add_mapper(send2, true).await.unwrap();

        // Add a mapping. Mapper 1 succeeds, mapper 2 fails.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test")
            .unwrap()
            .into();
        let params = MappingParams {
            range: MemoryRange::new(0..0x1000),
            backing: MappingBacking::File {
                mappable,
                file_offset: 0,
            },
            writable: true,
            mapping_type: MappingType::Device,
            policy: MemoryPolicy::none(),
        };

        let add_future = task.add_mapping(params);
        let (result, _, _) = futures::join!(
            add_future,
            async {
                // Mapper 1: succeed.
                let msg = recv1.recv().await.unwrap();
                match msg {
                    MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                    other => panic!("expected MapEager, got {other:?}"),
                }
                // Mapper 1 should then receive an Unmap (rollback).
                let msg = recv1.recv().await.unwrap();
                match msg {
                    MapperRequest::Unmap(rpc) => {
                        let (range, rpc) = rpc.split();
                        assert_eq!(range, MemoryRange::new(0..0x1000));
                        rpc.complete(());
                    }
                    other => panic!("expected Unmap rollback, got {other:?}"),
                }
            },
            async {
                // Mapper 2: fail.
                let msg = recv2.recv().await.unwrap();
                match msg {
                    MapperRequest::MapEager(rpc) => {
                        rpc.complete(Err(RemoteError::new(std::io::Error::other(
                            "simulated failure",
                        ))));
                    }
                    other => panic!("expected MapEager, got {other:?}"),
                }
            }
        );

        // add_mapping should return an error.
        assert!(result.is_err());

        // The mapping should not have been added.
        assert!(task.mappings.is_empty());
    }

    #[pal_async::async_test]
    async fn test_upgrade_to_eager_rollback_on_failure(_spawn: impl Spawn) {
        let mut task = task_with_two_mappings().await;

        // Add a lazy mapper.
        let (send, mut recv) = mesh::channel();
        let id = task.add_mapper(send, false).await.unwrap();
        assert!(!task.mappers.mappers[id.0].eager);

        // Upgrade: succeed on first mapping, fail on second.
        let upgrade_future = task.upgrade_to_eager(id);
        let (result, _) = futures::join!(upgrade_future, async {
            // First MapEager: succeed.
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => rpc.complete(Ok(())),
                other => panic!("expected MapEager #1, got {other:?}"),
            }
            // Second MapEager: fail.
            let msg = recv.recv().await.unwrap();
            match msg {
                MapperRequest::MapEager(rpc) => {
                    rpc.complete(Err(RemoteError::new(std::io::Error::other(
                        "simulated failure",
                    ))));
                }
                other => panic!("expected MapEager #2, got {other:?}"),
            }
        });

        // upgrade_to_eager should return an error.
        assert!(result.is_err());

        // The mapper should still be lazy (rolled back).
        assert!(!task.mappers.mappers[id.0].eager);

        // The first mapping should still have this mapper in active_mappers
        // (it was successfully replayed), so it will get Unmap when that
        // mapping is removed.
        assert!(
            task.mappings[0].active_mappers.contains(&id),
            "first mapping should retain mapper in active_mappers"
        );
        assert!(
            !task.mappings[1].active_mappers.contains(&id),
            "second mapping should not have mapper (replay failed)"
        );

        // The mapper should still be in the slab (not removed, just stayed lazy).
        assert!(task.mappers.mappers.contains(id.0));

        // Behavioral check: a subsequent add_mapping should NOT push to
        // this mapper (it's still lazy).
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test")
            .unwrap()
            .into();
        task.add_mapping(MappingParams {
            range: MemoryRange::new(0x20000..0x21000),
            backing: MappingBacking::File {
                mappable,
                file_offset: 0,
            },
            writable: true,
            mapping_type: MappingType::Device,
            policy: MemoryPolicy::none(),
        })
        .await
        .unwrap();

        // The lazy mapper should have received nothing.
        assert!(
            recv.try_recv().is_err(),
            "lazy mapper should not receive add_mapping push after failed upgrade"
        );
    }

    #[pal_async::async_test]
    async fn test_eager_page_fault_fails_immediately(_spawn: impl Spawn) {
        use super::super::va_mapper::VaMapper;

        // Create a VaMapper directly, manually driving the AddMapper RPC.
        let (req_send, mut req_recv) = mesh::channel::<MappingRequest>();
        let mapper_future = VaMapper::new(
            req_send,
            0x10000,
            None,
            None,
            true, // eager
            MapperRole::Primary,
        );
        let (mapper, _) = futures::join!(mapper_future, async {
            let msg = req_recv.recv().await.unwrap();
            match msg {
                MappingRequest::AddMapper(rpc) => {
                    rpc.handle_failable_sync(|params| {
                        assert!(params.eager);
                        Ok::<_, MappingError>(MapperId(0))
                    });
                }
                _ => panic!("expected AddMapper"),
            }
        });
        let mapper = mapper.unwrap();
        assert!(mapper.is_eager());

        // No mappings have been established, so a page fault on a
        // file-backed address should fail immediately rather than
        // trying to request the mapping lazily.
        let action = mapper.page_fault(0x1000, 0x1000, false, false);
        assert!(
            matches!(action, guestmem::PageFaultAction::Fail(_)),
            "eager mapper should fail page faults on unmapped file-backed ranges"
        );
    }

    #[pal_async::async_test]
    async fn test_va_mapper_drop_removes_mapper(_spawn: impl Spawn) {
        use super::super::va_mapper::VaMapper;

        let (req_send, mut req_recv) = mesh::channel::<MappingRequest>();
        let mapper_future = VaMapper::new(
            req_send,
            0x10000,
            None,
            None,
            true, // eager
            MapperRole::Primary,
        );
        let (mapper, mapper_req_send) = futures::join!(mapper_future, async {
            let msg = req_recv.recv().await.unwrap();
            match msg {
                MappingRequest::AddMapper(rpc) => {
                    let (params, rpc) = rpc.split();
                    assert!(params.eager);
                    rpc.complete(Ok(MapperId(7)));
                    params.send
                }
                _ => panic!("expected AddMapper"),
            }
        });
        drop(mapper.unwrap());

        match req_recv.recv().await.unwrap() {
            MappingRequest::RemoveMapper(id) => assert_eq!(id, MapperId(7)),
            _ => panic!("expected RemoveMapper"),
        }

        // In production, MappingManagerTask::remove_mapper drops this sender.
        // Do the same here so the mapper thread can exit.
        drop(mapper_req_send);
    }

    #[pal_async::async_test]
    async fn test_lazy_page_fault_requests_mapping(spawn: impl Spawn) {
        let _ = spawn;
        let (manager_thread, manager_driver) =
            pal_async::DefaultPool::spawn_on_thread("mapping-manager-test");
        let mm = MappingManager::new_without_primary(&manager_driver, 0x10000, None);
        let client = mm.client().clone();

        // Add a mapping so the lazy mapper can find it.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x10000, "test")
            .unwrap()
            .into();
        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0..0x10000),
                backing: MappingBacking::File {
                    mappable,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Device,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        // Create a lazy mapper.
        let mapper = client.new_mapper(false).await.unwrap();
        assert!(!mapper.is_eager());

        // page_fault calls block_on(request_mapping(...)), which blocks the
        // calling thread. The MappingManager task is running on its own pool
        // thread, so it can still serve SendMappings while this test thread is
        // blocked here.
        let action = mapper.page_fault(0x1000, 0x1000, false, false);
        assert!(
            matches!(action, guestmem::PageFaultAction::Retry),
            "lazy mapper should request mapping on page fault and succeed"
        );

        drop(mapper);
        drop(client);
        drop(mm);
        drop(manager_driver);
        manager_thread.join().unwrap();
    }

    /// Tests that creating an eager mapper succeeds even when mappings
    /// already exist (the mapper thread must be running to service the
    /// replay RPCs during AddMapper).
    #[pal_async::async_test]
    async fn test_eager_mapper_with_existing_mappings(spawn: impl Spawn) {
        let _ = spawn;
        let (manager_thread, manager_driver) =
            pal_async::DefaultPool::spawn_on_thread("mapping-manager-test");
        let mm = MappingManager::new_without_primary(&manager_driver, 0x10000, None);
        let client = mm.client().clone();

        // Add a mapping while no mappers exist — it is stored for replay.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x10000, "test")
            .unwrap()
            .into();
        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0..0x10000),
                backing: MappingBacking::File {
                    mappable,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Ram,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        // Create an eager mapper. The mapper thread must be spawned before
        // the AddMapper RPC so it can respond to replay MapEager RPCs.
        let mapper = client.new_mapper(true).await.unwrap();
        assert!(mapper.is_eager());

        drop(mapper);
        drop(client);
        drop(mm);
        drop(manager_driver);
        manager_thread.join().unwrap();
    }

    #[pal_async::async_test]
    async fn test_new_mapper_upgrades_cached_lazy_to_eager(spawn: impl Spawn) {
        let _ = spawn;
        let (manager_thread, manager_driver) =
            pal_async::DefaultPool::spawn_on_thread("mapping-manager-test");
        let mm = MappingManager::new_without_primary(&manager_driver, 0x20000, None);
        let client = mm.client().clone();

        // Add a mapping first so replay has something to push.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x10000, "test")
            .unwrap()
            .into();
        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0..0x10000),
                backing: MappingBacking::File {
                    mappable,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Device,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        // Create a lazy mapper first — this gets cached.
        let lazy = client.new_mapper(false).await.unwrap();
        assert!(!lazy.is_eager());

        // Now request an eager mapper — should upgrade the cached lazy one.
        let eager = client.new_mapper(true).await.unwrap();

        // They should be the same Arc (same underlying mapper).
        assert!(Arc::ptr_eq(&lazy, &eager));

        // Queue one more eager mapping. The manager sends SetEager before
        // returning from the upgrade RPC, and this MapEager is queued after
        // that, so a successful add_mapping means the mapper thread has
        // processed SetEager.
        let mappable: Mappable = sparse_mmap::alloc_shared_memory(0x1000, "test")
            .unwrap()
            .into();
        client
            .add_mapping(MappingParams {
                range: MemoryRange::new(0x10000..0x11000),
                backing: MappingBacking::File {
                    mappable,
                    file_offset: 0,
                },
                writable: true,
                mapping_type: MappingType::Device,
                policy: MemoryPolicy::none(),
            })
            .await
            .unwrap();

        // The mapper is now observably eager.
        assert!(eager.is_eager());

        drop(eager);
        drop(lazy);
        drop(client);
        drop(mm);
        drop(manager_driver);
        manager_thread.join().unwrap();
    }
}
