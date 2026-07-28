// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implements the VA mapper, which maintains a linear virtual address space for
//! all memory mapped into a partition.
//!
//! VA mappers come in two modes:
//!
//! - **Eager**: mappings are pushed by the mapping manager when they are added
//!   and replayed when the mapper is created. Page faults on file-backed ranges
//!   fail immediately — the mapping should already be established. This is the
//!   right mode for the VP process, where hypervisors like KVM do not forward
//!   page faults back to the VMM.
//!
//! - **Lazy**: mappings are not pushed proactively. Instead, page faults
//!   trigger an on-demand request to the mapping manager, which finds the
//!   backing mapping and pushes it to the mapper via Rpc. This avoids the cost
//!   of notifying processes that rarely access certain mappings (e.g.,
//!   device-emulation processes with virtio-fs DAX).
//!
//! In both modes, private memory ranges are committed up front (Windows) or
//! handled transparently by the kernel (Linux).
//!
//! On Windows, the **primary** (local) mapper's writable THP-eligible guest RAM
//! (private *and* shared/section) additionally uses a "deferred protect" scheme
//! for soft large pages: the range is committed/mapped read-only, and the first
//! write fault upgrades a full 2 MB window to read-write and prefetches it (via
//! `page_fault` for host-side writes such as the loader, or `resolve` for guest
//! writes). Faulting a uniform 2 MB region in one operation gives the OS the
//! opportunity to back it with a large page (which the hypervisor can map as a
//! 2 MB SLAT entry) instead of the fragmented small pages that result from
//! dribbled per-page writes. Non-primary (device/DMA) mappers use plain 4 KB
//! read-write pages.
//!
//! When such a range is also *prefetched*, it is populated eagerly at build
//! time instead: it stays read-write (the build-time populate cannot access a
//! read-only mapping) and its per-window first-fault bitmap starts fully set, so
//! `resolve` treats every window as already attempted.

// UNSAFETY: Implementing the unsafe GuestMemoryAccess trait by calling unsafe
// low level memory manipulation functions.
#![expect(unsafe_code)]

// Soft large pages are a Windows-only optimization; other targets get an
// uninhabited stub with the same interface (`SoftLp::new` returns `None`) so the
// fault paths compile without per-item `cfg`s.
#[cfg_attr(not(windows), path = "va_mapper/soft_lp_stub.rs")]
mod soft_lp;

use self::soft_lp::SoftLp;
use super::manager::DmaRegionProvider;
use super::manager::MapperId;
use super::manager::MapperRequest;
use super::manager::MappingBacking;
use super::manager::MappingError;
use super::manager::MappingParams;
use super::manager::MappingRequest;
use super::manager::MemoryPolicy;
use crate::RemoteProcess;
use futures::executor::block_on;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemoryBackingError;
use guestmem::GuestMemoryErrorKind;
use guestmem::GuestMemorySharing;
use guestmem::PageFaultAction;
use guestmem::PageFaultError;
use inspect::Inspect;
use inspect_counters::SharedCounter;
use memory_range::MemoryRange;
use mesh::error::RemoteError;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use parking_lot::Mutex;
use parking_lot::RwLock;
use range_map_vec::RangeMap;
use sparse_mmap::SparseMapping;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use thiserror::Error;
use virt::ResolveMemoryFault;
#[cfg(windows)]
use windows_sys::Win32::System::Memory::PAGE_READONLY;
#[cfg(windows)]
use windows_sys::Win32::System::Memory::PAGE_READWRITE;
#[cfg(windows)]
use windows_sys::Win32::System::Memory::SECTION_MAP_READ;
#[cfg(windows)]
use windows_sys::Win32::System::Memory::SECTION_MAP_WRITE;

#[derive(Debug, Error)]
#[error("unexpected page fault")]
struct UnexpectedPageFault;

/// The role of a [`VaMapper`].
///
/// Exactly one mapper per VM is [`Primary`](Self::Primary): the loader's write
/// target and the partition's fault resolver, and the only mapper for which soft
/// large pages (Windows) are worthwhile, since its host backing drives the
/// guest's SLAT. All other guest-memory access — `guest_memory()` in any
/// process, DMA mappers, and remote partition-backing mappers — is
/// [`Secondary`](Self::Secondary) and uses plain read-write 4 KB pages.
///
/// This is a role, not a location: it is set explicitly at construction rather
/// than inferred from whether the mapping is local, so a remote primary mapper
/// or a local secondary mapper (e.g. a device process's own local mapper) is
/// handled correctly.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum MapperRole {
    /// The single loader/partition mapper; eligible for soft large pages.
    Primary {
        /// Whether the partition delivers guest-memory-access faults to the
        /// VMM. Soft large pages map a window read-only and rely on the
        /// resulting write fault being resolved to raise it, so they are only
        /// enabled when this is set. Carried on the `Primary` variant because it
        /// is meaningless for a secondary mapper.
        supports_memory_fault_resolution: bool,
    },
    /// Any other mapper; plain read-write 4 KB pages.
    Secondary,
}

/// Properties recorded for each active guest-memory mapping, used to answer
/// per-address queries (private vs. shared, soft-large-page state) without a
/// static snapshot of the RAM layout.
#[derive(Debug)]
struct MappingProps {
    /// Backed by private anonymous memory (committed up front) rather than a
    /// shared file/section mapping.
    private: bool,
    /// General per-mapping fault counters, always present. See [`FaultStats`].
    stats: FaultStats,
    /// Soft-large-page (Windows THP) state, or `None` when the scheme does not
    /// apply (non-primary/device mappers, read-only or non-THP ranges, and every
    /// non-Windows host). See the [`soft_lp`] module.
    soft_lp: Option<SoftLp>,
}

/// Per-mapping fault counters, exposed via `Inspect`.
///
/// Recorded for every mapping regardless of host OS, role, or backing, so
/// general fault accounting is available even on mappings that never use soft
/// large pages. Kept per mapping — one set of counters per backing, and thus
/// per NUMA node — so they scale for large multi-NUMA-node VMs rather than
/// contending on a single global counter. The counters are plain atomics
/// (`SharedCounter`), bumped in place under the mapping-index read lock.
#[derive(Debug, Default, Inspect)]
struct FaultStats {
    /// Guest memory faults resolved for this mapping.
    guest_faults: SharedCounter,
}

/// A virtual address space mapper for guest memory.
///
/// Maintains a reserved VA range and maps file-backed or anonymous memory
/// into it as directed by the mapping manager.
pub struct VaMapper {
    inner: Arc<MapperInner>,
    id: MapperId,
    process: Option<RemoteProcess>,
    _thread: JoinHandle<()>,
}

impl std::fmt::Debug for VaMapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaMapper")
            .field("inner", &self.inner)
            .field("_thread", &self._thread)
            .finish()
    }
}

impl Drop for VaMapper {
    fn drop(&mut self) {
        // Do not join the mapper thread here. The mapping manager must process
        // this request before the mapper request channel closes, and joining in
        // Drop could deadlock if the manager task needs the current executor to
        // make progress. Once the manager removes its sender, the mapper thread
        // exits naturally.
        self.inner
            .req_send
            .send(MappingRequest::RemoveMapper(self.id));
    }
}

impl Inspect for VaMapper {
    /// Contributes each mapping's counters to the shared `mappings` node, keyed
    /// by GPA range, so the stats sit alongside the mapping they describe (the
    /// mapping-manager entry with the same range merges with this one). Every
    /// mapping contributes a `faults` child (general fault accounting); only
    /// soft-large-page mappings (the primary mapper's writable THP ranges on
    /// Windows) additionally contribute a `soft_large_pages` child.
    fn inspect(&self, req: inspect::Request<'_>) {
        req.respond().field(
            "mappings",
            inspect::adhoc(|req| {
                let mut resp = req.respond();
                let mappings = self.inner.mappings.read();
                for (range, props) in mappings.iter() {
                    let range = MemoryRange::new(*range.start()..*range.end() + 1);
                    resp.field(
                        &range.to_string(),
                        inspect::adhoc(|req| {
                            let mut resp = req.respond();
                            resp.field("faults", &props.stats);
                            if let Some(sl) = &props.soft_lp {
                                resp.field("soft_large_pages", sl);
                            }
                        }),
                    );
                }
            }),
        );
    }
}

#[derive(Debug)]
struct MapperInner {
    mapping: SparseMapping,
    /// Waiters for lazy mapping requests. `None` after the mapper task exits.
    waiters: Mutex<Option<Vec<MapWaiter>>>,
    /// Index of active mappings recorded as they are established, keyed by GPA.
    /// Written by the mapper task on map/unmap and read by the page-fault and
    /// fault-resolution paths. Replaces a static snapshot of the RAM layout, so
    /// hot-added ranges populate it like any other mapping.
    mappings: RwLock<RangeMap<u64, MappingProps>>,
    /// Whether this mapper receives mappings eagerly (pushed by the
    /// mapping manager) or lazily (on demand via page faults).
    /// Set by the mapping manager task after replay is complete.
    ///
    /// `Relaxed` ordering is sufficient: this flag is only read by the
    /// page-fault handler to decide between eager-fail and lazy-request
    /// paths. A stale `false` (lazy) is harmless — the lazy path
    /// succeeds because the mapping is already established. The flag
    /// is eventually updated after `SetEager` is processed.
    eager: AtomicBool,
    /// Whether this is the **primary** mapper — the one the partition and the
    /// loader run against. Soft large pages (Windows) are only worthwhile here,
    /// since this is the mapping whose host backing drives the guest's SLAT.
    /// Secondary mappers use plain read-write 4 KB pages. See [`MapperRole`].
    primary: bool,
    /// Whether the partition delivers guest-memory-access faults to the VMM.
    /// Soft large pages map a window read-only and rely on the resulting write
    /// fault being resolved to raise it, so without fault resolution they would
    /// wedge on the first guest write; the primary mapper only enables them when
    /// this is set.
    supports_memory_fault_resolution: bool,
    req_send: mesh::Sender<MappingRequest>,
}

/// A pending lazy mapping request.
#[derive(Debug)]
struct MapWaiter {
    range: MemoryRange,
    writable: bool,
    done: mesh::OneshotSender<bool>,
}

impl MapWaiter {
    /// Check whether the established mapping satisfies this waiter.
    /// Returns `Some(true)` if fully satisfied, `Some(false)` if the
    /// mapping doesn't meet requirements (e.g., read-only when write
    /// needed), or `None` if the waiter still has remaining range.
    fn complete(&mut self, range: MemoryRange, writable: Option<bool>) -> Option<bool> {
        if range.contains_addr(self.range.start()) {
            if writable.is_none() || (self.writable && writable == Some(false)) {
                return Some(false);
            }
            let new_start = self.range.end().min(range.end());
            let remaining = MemoryRange::new(new_start..self.range.end());
            if remaining.is_empty() {
                return Some(true);
            }
            tracing::debug!(%remaining, "waiting for more");
            self.range = remaining;
        }
        None
    }
}

struct MapperTask {
    inner: Arc<MapperInner>,
}

impl MapperTask {
    async fn run(mut self, mut req_recv: mesh::Receiver<MapperRequest>) {
        while let Ok(req) = req_recv.recv().await {
            match req {
                MapperRequest::Unmap(rpc) => rpc.handle_sync(|range| {
                    tracing::debug!(%range, "invalidate received");
                    self.inner
                        .mapping
                        .unmap(range.start() as usize, range.len() as usize)
                        .expect("invalidate request should be valid");
                    self.inner.remove_mapping(range);
                }),
                MapperRequest::MapEager(rpc) => {
                    rpc.handle_failable_sync(|params| {
                        tracing::debug!(range = %params.range, "eager mapping received");
                        self.map(params)
                    });
                }
                MapperRequest::MapLazy(params) => {
                    tracing::debug!(range = %params.range, "lazy mapping received");
                    let (range, writable) = (params.range, params.writable);
                    match self.map(params) {
                        Ok(()) => self.wake_waiters(range, Some(writable)),
                        Err(e) => {
                            tracing::error!(
                                error = &e as &dyn std::error::Error,
                                %range,
                                "failed to map file for range"
                            );
                            self.wake_waiters(range, None);
                        }
                    }
                }
                MapperRequest::NoMapping(range) => {
                    // Wake up waiters. They'll see a failure when they try
                    // to access the VA.
                    tracing::debug!(%range, "no mapping received for range");
                    self.wake_waiters(range, None);
                }
                MapperRequest::SetEager(rpc) => rpc.handle_sync(|()| {
                    tracing::debug!("mapper upgraded to eager");
                    self.inner.eager.store(true, Ordering::Relaxed);
                }),
            }
        }
        // Don't allow more waiters.
        *self.inner.waiters.lock() = None;
        // Invalidate everything.
        let _ = self.inner.mapping.unmap(0, self.inner.mapping.len());
    }

    /// Establishes a mapping in the VA space, dispatching on how it is backed.
    fn map(&self, params: MappingParams) -> Result<(), MappingError> {
        // Soft large pages apply only to writable THP-eligible RAM on the primary
        // mapper (Windows); `SoftLp::new` returns `None` otherwise. See the
        // `soft_lp` module. They also depend on the partition delivering write
        // faults to raise deferred-protect windows, so don't even build one when
        // the partition can't resolve faults.
        let soft_lp = if self.inner.supports_memory_fault_resolution {
            SoftLp::new(
                params.range,
                &params.policy,
                params.writable,
                self.inner.primary,
            )
        } else {
            None
        };

        // Deferred protect (map read-only, raise on the first write fault) drives
        // the lazy soft-LP path; prefetched ranges are populated read-write
        // eagerly at build time instead.
        let deferred_protect = soft_lp.as_ref().is_some_and(SoftLp::deferred_protect);

        let private = match &params.backing {
            MappingBacking::File {
                mappable,
                file_offset,
            } => {
                self.map_file(&params, mappable, *file_offset, deferred_protect)?;
                false
            }
            MappingBacking::Private => {
                self.map_private(&params, deferred_protect)?;
                true
            }
        };
        self.inner.record_mapping(
            params.range,
            MappingProps {
                private,
                stats: FaultStats::default(),
                soft_lp,
            },
        );
        Ok(())
    }

    /// Maps a file-backed region into the VA space, applying NUMA policy where
    /// supported.
    fn map_file(
        &self,
        params: &MappingParams,
        mappable: &super::mappable::Mappable,
        file_offset: u64,
        deferred_protect: bool,
    ) -> Result<(), MappingError> {
        let &MappingParams {
            range,
            backing: _,
            writable,
            mapping_type: _,
            policy:
                MemoryPolicy {
                    numa_node,
                    transparent_hugepages,
                    prefetch: _,
                },
        } = params;
        // A deferred-protect range is mapped read-write (so the view has write
        // access and its pages can be raised back to read-write on the first
        // write fault) and then immediately protected down to read-only just
        // below. Mapping the view read-only up front instead would create a view
        // whose pages cannot be raised to read-write later (`VirtualProtect`
        // fails with ERROR_INVALID_PARAMETER). Deferred protect implies
        // `writable`.
        #[cfg(windows)]
        let (protect, access) = (
            if writable {
                PAGE_READWRITE
            } else {
                PAGE_READONLY
            },
            if writable {
                SECTION_MAP_READ | SECTION_MAP_WRITE
            } else {
                SECTION_MAP_READ
            },
        );
        // `deferred_protect` is only consulted on Windows below; keep it live on
        // other targets so the shared parameter doesn't warn.
        let _ = deferred_protect;
        let map_result = cfg_select! {
            windows => {
                self.inner.mapping.map_view_of_file_access(
                    range.start() as usize,
                    range.len() as usize,
                    mappable,
                    file_offset,
                    protect,
                    access,
                    numa_node,
                )
            }
            _ => {
                self.inner.mapping.map_file(
                    range.start() as usize,
                    range.len() as usize,
                    mappable,
                    file_offset,
                    writable,
                )
            }
        };

        if let Err(e) = map_result {
            return Err(MappingError::new(range, e));
        }

        // Deferred protect: lower the freshly-mapped writable view to read-only
        // so the first write faults; `page_fault`/`resolve` then raise the
        // touched 2 MB window back to read-write.
        #[cfg(windows)]
        if deferred_protect {
            if let Err(e) = self.inner.mapping.protect(
                range.start() as usize,
                range.len() as usize,
                PAGE_READONLY,
            ) {
                return Err(MappingError::new(range, e));
            }
        }

        // Mark shared (file-backed) RAM as THP-eligible. This is advisory:
        // on Linux the kernel honors it for shmem/tmpfs (memfd) mappings
        // according to `/sys/kernel/mm/transparent_hugepage/shmem_enabled`.
        // The kernel may accept the advice without allocating huge pages;
        // advice failures are logged but do not fail the mapping.
        #[cfg(target_os = "linux")]
        if transparent_hugepages {
            if let Err(e) = self
                .inner
                .mapping
                .madvise_hugepage(range.start() as usize, range.len() as usize)
            {
                tracing::warn!(
                    error = &e as &dyn std::error::Error,
                    %range,
                    "failed to mark shared RAM as THP eligible"
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = transparent_hugepages;

        cfg_select! {
            target_os = "linux" => {
                if let Some(node) = numa_node {
                    if let Err(e) = self.inner.mapping.mbind_at(
                        range.start() as usize,
                        range.len() as usize,
                        node,
                    ) {
                        tracing::error!(
                            error = &e as &dyn std::error::Error,
                            %range,
                            node,
                            "NUMA binding failed, using default placement"
                        );
                    }
                }
            }
            windows => {
                // NUMA handled by the map_view_of_file_access call above.
                let _ = numa_node;
            }
            _ => {
                assert!(numa_node.is_none(), "NUMA not supported on this platform; should have been rejected at build time");
            }
        }

        Ok(())
    }

    /// Commits private anonymous memory for a range into the VA space.
    ///
    /// This replaces the reserved placeholder at `range` with committed
    /// anonymous pages, optionally bound to a host NUMA node and marked
    /// eligible for Transparent Huge Pages.
    fn map_private(
        &self,
        params: &MappingParams,
        deferred_protect: bool,
    ) -> Result<(), MappingError> {
        let &MappingParams {
            range,
            backing: _,
            writable: _,
            mapping_type: _,
            policy:
                MemoryPolicy {
                    numa_node,
                    transparent_hugepages,
                    prefetch: _,
                },
        } = params;
        let offset = range.start() as usize;
        let len = range.len() as usize;

        // On Windows, deferred-protect private RAM commits read-only so the first
        // write faults and a full 2 MB window can be raised to read-write and
        // materialized at once, giving the loader (and guest) large pages.
        // Elsewhere this flag is ignored.
        if let Err(e) = self.inner.alloc(offset, len, numa_node, deferred_protect) {
            return Err(MappingError::new(range, e));
        }

        // Name the range so it's identifiable in /proc/{pid}/smaps.
        self.inner
            .mapping
            .set_name(offset, len, "guest-ram-private");

        #[cfg(target_os = "linux")]
        if transparent_hugepages {
            if let Err(e) = self.inner.mapping.madvise_hugepage(offset, len) {
                tracing::warn!(
                    error = &e as &dyn std::error::Error,
                    %range,
                    "failed to mark private RAM as THP eligible"
                );
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = transparent_hugepages;

        Ok(())
    }

    fn wake_waiters(&mut self, range: MemoryRange, writable: Option<bool>) {
        let mut waiters = self.inner.waiters.lock();
        let waiters = waiters.as_mut().unwrap();

        let mut i = 0;
        while i < waiters.len() {
            if let Some(success) = waiters[i].complete(range, writable) {
                waiters.swap_remove(i).done.send(success);
            } else {
                i += 1;
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum VaMapperError {
    #[error("failed to communicate with the memory manager")]
    MemoryManagerGone(#[source] RpcError),
    #[error("failed to register mapper")]
    Registration(#[source] RemoteError),
    #[error("failed to reserve address space")]
    Reserve(#[source] std::io::Error),
}

/// Error returned when a lazy mapping request cannot be fulfilled.
#[derive(Debug, Error)]
#[error("no mapping for {0}")]
pub struct NoMapping(MemoryRange);

impl MapperInner {
    /// Records an established mapping in the index, replacing any stale entry
    /// for the same range.
    fn record_mapping(&self, range: MemoryRange, props: MappingProps) {
        if range.is_empty() {
            return;
        }
        let mut mappings = self.mappings.write();
        mappings.remove_range(range.start()..=range.end() - 1);
        let inserted = mappings.insert(range.start()..=range.end() - 1, props);
        assert!(
            inserted,
            "mapping index range should be clear after removal"
        );
    }

    /// Removes a mapping from the index.
    fn remove_mapping(&self, range: MemoryRange) {
        if range.is_empty() {
            return;
        }
        self.mappings
            .write()
            .remove_range(range.start()..=range.end() - 1);
    }

    /// Request that the mapping manager send mappings for the given range.
    ///
    /// Registers a waiter, sends `SendMappings` (fire-and-forget), and
    /// awaits the waiter oneshot. The mapping manager will send `MapLazy`
    /// or `NoMapping` messages to the mapper task, which wakes the waiter.
    async fn request_mapping(
        &self,
        id: MapperId,
        range: MemoryRange,
        writable: bool,
    ) -> Result<(), NoMapping> {
        let (send, recv) = mesh::oneshot();
        self.waiters
            .lock()
            .as_mut()
            .ok_or(NoMapping(range))?
            .push(MapWaiter {
                range,
                writable,
                done: send,
            });

        tracing::debug!(%range, "waiting for mappings");
        self.req_send.send(MappingRequest::SendMappings(id, range));
        match recv.await {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(NoMapping(range)),
        }
    }

    /// Commits private anonymous memory for a range, optionally bound to a
    /// specific host NUMA node.
    ///
    /// This replaces the placeholder at the given offset with committed
    /// anonymous memory.
    ///
    /// When `deferred_protect` is set (Windows soft large pages), the memory is
    /// committed read-only instead of read-write, so the first write faults and
    /// [`VaMapper`] can upgrade a full 2 MB window to read-write at once. This
    /// has no effect on other platforms.
    ///
    /// Caution: on Linux, if NUMA binding fails, the allocation itself has
    /// still succeeded — the returned error does not imply the memory is
    /// unmapped.
    fn alloc(
        &self,
        offset: usize,
        len: usize,
        numa_node: Option<u32>,
        deferred_protect: bool,
    ) -> Result<(), std::io::Error> {
        cfg_select! {
            windows => {
                // Deferred protect (soft large pages): commit read-only so the
                // first write faults and the 2 MB window can be raised +
                // materialized as a large page; otherwise commit read-write.
                let protect = if deferred_protect {
                    PAGE_READONLY
                } else {
                    PAGE_READWRITE
                };
                self.mapping.virtual_alloc(offset, len, protect, numa_node)
            }
            target_os = "linux" => {
                let _ = deferred_protect;
                self.mapping.alloc(offset, len)?;
                if let Some(node) = numa_node {
                    self.mapping.mbind_at(offset, len, node)?;
                }
                Ok(())
            }
            _ => {
                let _ = deferred_protect;
                assert!(numa_node.is_none(), "NUMA not supported on this platform; should have been rejected at build time");
                self.mapping.alloc(offset, len)
            }
        }
    }
}

impl VaMapper {
    pub(crate) async fn new(
        req_send: mesh::Sender<MappingRequest>,
        len: u64,
        remote_process: Option<RemoteProcess>,
        minimum_alignment: Option<usize>,
        eager: bool,
        role: MapperRole,
    ) -> Result<Self, VaMapperError> {
        // Soft large pages apply only to the primary mapper, and only when the
        // partition resolves faults; `supports_memory_fault_resolution` rides on
        // the `Primary` variant.
        let (primary, supports_memory_fault_resolution) = match role {
            MapperRole::Primary {
                supports_memory_fault_resolution,
            } => (true, supports_memory_fault_resolution),
            MapperRole::Secondary => (false, false),
        };
        let mapping = match &remote_process {
            None => SparseMapping::new_with_minimum_alignment(
                len as usize,
                minimum_alignment.unwrap_or(1),
            ),
            Some(process) => match process {
                #[cfg(not(windows))]
                _ => unreachable!(),
                #[cfg(windows)]
                process => SparseMapping::new_remote(
                    process.as_handle().try_clone_to_owned().unwrap().into(),
                    None,
                    len as usize,
                    minimum_alignment.unwrap_or(1),
                ),
            },
        }
        .map_err(VaMapperError::Reserve)?;

        // Name the VA reservation so it's identifiable in /proc/{pid}/smaps.
        mapping.set_name(0, mapping.len(), "guest-memory");

        let (send, req_recv) = mesh::channel();

        let inner = Arc::new(MapperInner {
            mapping,
            waiters: Mutex::new(Some(Vec::new())),
            mappings: RwLock::new(RangeMap::new()),
            eager: AtomicBool::new(eager),
            primary,
            supports_memory_fault_resolution,
            req_send,
        });

        // Spawn the mapper thread *before* the AddMapper RPC. The manager
        // replays existing mappings to eager mappers during AddMapper, so
        // the mapper thread must be running to respond to those RPCs.
        //
        // FUTURE: use a task once we resolve the block_ons in the
        // GuestMemoryAccess implementation.
        let thread = std::thread::Builder::new()
            .name("mapper".to_owned())
            .spawn({
                let runner = MapperTask {
                    inner: inner.clone(),
                };
                || block_on(runner.run(req_recv))
            })
            .unwrap();

        let id = match inner
            .req_send
            .call(
                MappingRequest::AddMapper,
                super::manager::AddMapperParams { send, eager },
            )
            .await
        {
            Ok(Ok(id)) => id,
            Ok(Err(e)) => {
                // Drop inner to shut down the mapper thread (closes req_recv).
                drop(inner);
                let _ = thread.join();
                return Err(VaMapperError::Registration(e));
            }
            Err(e) => {
                drop(inner);
                let _ = thread.join();
                return Err(VaMapperError::MemoryManagerGone(e));
            }
        };

        Ok(VaMapper {
            inner,
            id,
            process: remote_process,
            _thread: thread,
        })
    }

    /// Returns the base pointer of the VA reservation.
    pub fn as_ptr(&self) -> *mut u8 {
        self.inner.mapping.as_ptr().cast()
    }

    /// Returns the length of the VA reservation in bytes.
    pub fn len(&self) -> usize {
        self.inner.mapping.len()
    }

    /// Returns true if this mapper receives mappings eagerly.
    pub fn is_eager(&self) -> bool {
        self.inner.eager.load(Ordering::Relaxed)
    }

    /// Returns the mapper's ID, used internally for upgrade requests.
    pub(crate) fn mapper_id(&self) -> MapperId {
        self.id
    }

    /// Returns the remote process, if this mapper maps into a remote process.
    pub fn process(&self) -> Option<&RemoteProcess> {
        self.process.as_ref()
    }
}

/// SAFETY: the underlying VA mapping is guaranteed to be valid for the lifetime
/// of this object.
unsafe impl GuestMemoryAccess for VaMapper {
    fn mapping(&self) -> Option<NonNull<u8>> {
        // No one should be using this as a GuestMemoryAccess for remote
        // mappings, but it's convenient to have the same type for both local
        // and remote mappings for the sake of simplicity in
        // `PartitionRegionMapper`.
        assert!(self.inner.mapping.is_local());

        NonNull::new(self.inner.mapping.as_ptr().cast())
    }

    fn max_address(&self) -> u64 {
        self.inner.mapping.len() as u64
    }

    fn page_fault(
        &self,
        address: u64,
        len: usize,
        write: bool,
        bitmap_failure: bool,
    ) -> PageFaultAction {
        assert!(!bitmap_failure, "bitmaps are not used");

        // Soft large pages (Windows): THP-eligible ranges on the primary mapper
        // are committed/mapped read-only, so the first *write* traps here (reads
        // are served by the zero page and don't fault). This is the loader's
        // path; `SoftLp::on_host_write` raises (and prefetches) the covering
        // 2 MB window, then the write is retried. The mapping-index read lock is
        // held across the raise; it only blocks a concurrent *writer* (a
        // structural map/unmap), which is rare.
        #[cfg(windows)]
        if write {
            let mappings = self.inner.mappings.read();
            if let Some(&(start, end, ref props)) = mappings.get_entry(&address) {
                if let Some(sl) = &props.soft_lp {
                    return match sl.on_host_write(&self.inner.mapping, address, start, end) {
                        Ok(()) => PageFaultAction::Retry,
                        Err(err) => PageFaultAction::Fail(PageFaultError::new(
                            GuestMemoryErrorKind::Other,
                            err,
                        )),
                    };
                }
            }
        }

        if self.inner.eager.load(Ordering::Relaxed) {
            // Eager mapper: file-backed mappings are established proactively.
            // If we get a page fault, the mapping was never set up or was
            // torn down.
            return PageFaultAction::Fail(PageFaultError::new(
                GuestMemoryErrorKind::OutOfRange,
                UnexpectedPageFault,
            ));
        }

        // Lazy mapper: request the mapping on demand from the mapping manager.
        let range = MemoryRange::bounding(address..address + len as u64);
        if let Err(err) = block_on(self.inner.request_mapping(self.id, range, write)) {
            return PageFaultAction::Fail(PageFaultError::new(
                GuestMemoryErrorKind::OutOfRange,
                err,
            ));
        }
        PageFaultAction::Retry
    }

    fn sharing(&self) -> Option<GuestMemorySharing> {
        // Private anonymous memory is committed on fault in the local process
        // and cannot be shared to a remote DMA process, so disable DMA sharing
        // whenever any recorded mapping is private. Derived from the mapping
        // index rather than a static flag so it tracks the actual backings.
        if self.inner.mappings.read().iter().any(|(_, p)| p.private) {
            return None;
        }
        Some(GuestMemorySharing::new(DmaRegionProvider {
            req_send: self.inner.req_send.clone(),
        }))
    }
}

impl ResolveMemoryFault for VaMapper {
    fn resolve(
        &self,
        fault: MemoryRange,
        write: bool,
    ) -> Result<MemoryRange, GuestMemoryBackingError> {
        if fault.end() > self.inner.mapping.len() as u64 {
            return Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                fault.start(),
                UnexpectedPageFault,
            ));
        }

        // Hold the mapping-index read lock across the fault resolution. This only
        // blocks a concurrent *writer* (a structural map/unmap), which is rare;
        // other faulting VPs are readers and proceed in parallel. `end` is the
        // inclusive last address of the mapping.
        let mappings = self.inner.mappings.read();
        let Some(&(start, end, ref props)) = mappings.get_entry(&fault.start()) else {
            return Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                fault.start(),
                UnexpectedPageFault,
            ));
        };
        // The trait contract requires the resolved range to stay within the
        // single uniform RAM region that covers `fault.start()`. Today the only
        // caller faults one page at a time, so a fault never spans two mappings;
        // guard against a future caller passing a wider range that starts in this
        // mapping but extends past its end (`end` is the inclusive last address).
        if fault.end() > end + 1 {
            return Err(GuestMemoryBackingError::new(
                GuestMemoryErrorKind::OutOfRange,
                fault.start(),
                UnexpectedPageFault,
            ));
        }
        props.stats.guest_faults.increment();

        // Soft large pages (Windows) raise the covering 2 MB window on the first
        // write and may resolve to the whole window; every other mapping (and
        // every non-Windows host) resolves to the single faulting page.
        match &props.soft_lp {
            Some(sl) => sl
                .resolve(&self.inner.mapping, fault, write, start, end)
                .map_err(|err| {
                    GuestMemoryBackingError::new(GuestMemoryErrorKind::Other, fault.start(), err)
                }),
            None => Ok(fault),
        }
    }
}

#[cfg(test)]
mod tests {
    use sparse_mmap::SparseMapping;

    /// Tests that private RAM pages can be allocated, written to, and read from.
    #[test]
    fn test_private_ram_alloc_write_read() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Allocate (commit) the first two pages.
        mapping.alloc(0, 2 * page_size).unwrap();

        // Write and read through SparseMapping methods.
        let data = [0xABu8; 128];
        mapping.write_at(0, &data).unwrap();

        let mut buf = [0u8; 128];
        mapping.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, data);

        // Verify zeros at an untouched offset within committed range.
        let mut zero_buf = [0xFFu8; 64];
        mapping.read_at(page_size, &mut zero_buf).unwrap();
        assert!(
            zero_buf.iter().all(|&b| b == 0),
            "untouched committed memory should be zeros"
        );
    }

    /// Tests that commit is idempotent (committing already-committed pages is
    /// a no-op).
    #[test]
    fn test_private_ram_commit_idempotent() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Alloc then commit the same range again.
        mapping.alloc(0, 2 * page_size).unwrap();
        mapping.commit(0, 2 * page_size).unwrap();
        mapping.commit(0, page_size).unwrap();

        // Write and read should work.
        let pattern = vec![0xEFu8; 64];
        mapping.write_at(0, &pattern).unwrap();
        let mut buf = vec![0u8; 64];
        mapping.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, pattern);
    }
}
