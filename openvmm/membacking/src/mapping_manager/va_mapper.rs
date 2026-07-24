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
use std::sync::atomic::AtomicU64;
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

/// A failure in one of the steps the fault handler takes to service a guest-RAM
/// fault on Windows. Each variant names the operation and carries the
/// underlying OS error as its [`source`](std::error::Error::source), so a
/// passed-up guest-memory error identifies the step that failed rather than
/// surfacing a bare OS error code.
#[cfg(windows)]
#[derive(Debug, Error)]
enum FaultError {
    /// Raising a window to read-write failed (deferred protect / soft large
    /// pages).
    #[error("failed to raise window {0} to read-write")]
    Protect(MemoryRange, #[source] std::io::Error),
}

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
    Primary,
    /// Any other mapper; plain read-write 4 KB pages.
    Secondary,
}

/// Properties recorded for each active guest-memory mapping, used to answer
/// per-address queries (private vs. shared, soft-large-page first-fault state)
/// without a static snapshot of the RAM layout.
#[derive(Debug)]
struct MappingProps {
    /// Backed by private anonymous memory (committed up front) rather than a
    /// shared file/section mapping.
    private: bool,
    /// General per-mapping fault counters, always present. See [`FaultStats`].
    stats: FaultStats,
    /// Soft-large-page state for this range's 2 MB regions. `Some` only for
    /// writable THP-eligible ranges on the **primary** (local) mapper on Windows
    /// (soft large pages); `None` otherwise — non-primary (device/DMA) mappers,
    /// non-THP ranges, and non-Windows hosts spend nothing. The fault paths
    /// claim a region, materialize backing, and bump counters while holding the
    /// mapping-index read lock.
    ///
    /// `Some` also marks the range (private or shared) as "deferred protect":
    /// committed/mapped read-only and upgraded to read-write — then materialized
    /// with a 2 MB prefetch — a window at a time on first write.
    soft_lp: Option<SoftLp>,
}

/// Per-mapping soft-large-page state: the first-fault bitmap plus the
/// large-page prefetch/promotion counters. Stored inline in [`MappingProps`]
/// and accessed by the fault paths under the mapping-index read lock.
#[derive(Debug)]
struct SoftLp {
    first_fault: FirstFault,
    stats: SoftLpStats,
}

/// Per-mapping counters for the soft-large-page (2 MB window) machinery,
/// exposed via `Inspect`. These track only the large-page-specific work —
/// prefetching and promoting 2 MB windows. General fault accounting (the faults
/// that drive this work) lives in [`FaultStats`].
///
/// Kept per mapping (one backing, and thus one set of counters, per NUMA node)
/// so they scale for large multi-NUMA-node VMs rather than contending on a
/// single global counter.
#[derive(Debug, Default, Inspect)]
struct SoftLpStats {
    /// 2 MB windows prefetched on the host fault path.
    host_prefetches: SharedCounter,
    /// 2 MB windows promoted (first-fault widen) on the guest fault path.
    guest_promotions: SharedCounter,
    /// 2 MB windows pre-populated eagerly at build time (prefetch).
    eager_promotions: SharedCounter,
    /// Best-effort prefetch failures (no free large page, or pre-Win11).
    prefetch_failures: SharedCounter,
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
    /// Host write faults that raised a soft-large-page deferred-protect window
    /// to read-write (e.g. the loader's first write to a window). Nonzero only
    /// for soft-large-page mappings (Windows, primary mapper, THP-eligible).
    deferred_protect_faults: SharedCounter,
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
                                resp.field("soft_large_pages", &sl.stats);
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
        // Soft large pages are a Windows-only, THP-only feature that matters only
        // on the **primary** mapper: the partition and the loader run against its
        // VA, so that is the only place a large host backing yields a 2 MB SLAT
        // entry. Non-primary (device/DMA) mappers, read-only mappings (e.g. ROM),
        // non-THP ranges, and non-Windows hosts use plain read-write 4 KB pages.
        let soft_lp = cfg!(windows)
            && params.policy.transparent_hugepages
            && params.writable
            && self.inner.primary;

        // Deferred protect (map read-only, populate lazily on the first write
        // fault) drives the *lazy* soft-LP path. When the range is prefetched it
        // is populated *eagerly* at build time instead, so it stays read-write
        // (the build-time populate cannot access a read-only mapping) and its
        // 2 MB windows start already-attempted.
        let prefetch = soft_lp && params.policy.prefetch;
        let deferred_protect = soft_lp && !prefetch;

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
                soft_lp: soft_lp.then(|| {
                    let regions = large_region_count(params.range);
                    let stats = SoftLpStats::default();
                    // Prefetched ranges are populated up front, so mark every
                    // window attempted and count them as eager promotions; lazy
                    // ranges start unattempted.
                    let first_fault = if prefetch {
                        stats.eager_promotions.add(regions);
                        FirstFault::new_all_claimed(regions)
                    } else {
                        FirstFault::new(regions)
                    };
                    SoftLp { first_fault, stats }
                }),
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
            primary: role == MapperRole::Primary,
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
        // path: raise the whole 2 MB window to read-write and prefetch it so the
        // OS can back it with a contiguous large page, then retry the write.
        // Applies to both private and shared (section) RAM.
        //
        // The mapping-index read lock is held across the protect/prefetch
        // syscalls. This only blocks a concurrent *writer* (a structural
        // map/unmap), which is rare; other faulting VPs are readers and proceed
        // in parallel.
        #[cfg(windows)]
        if write {
            let mappings = self.inner.mappings.read();
            if let Some(&(start, end, ref props)) = mappings.get_entry(&address) {
                if let Some(sl) = &props.soft_lp {
                    // A hit on the write-fault path is a deferred-protect fault.
                    props.stats.deferred_protect_faults.increment();
                    let win_base = address & !(LARGE_PAGE_SIZE - 1);
                    let prot_start = win_base.max(start);
                    let prot_end = (win_base + LARGE_PAGE_SIZE).min(end + 1);
                    let off = prot_start as usize;
                    let plen = (prot_end - prot_start) as usize;
                    // Raise to read-write (idempotent) so the write proceeds.
                    if let Err(err) = self.inner.mapping.protect(off, plen, PAGE_READWRITE) {
                        return PageFaultAction::Fail(PageFaultError::new(
                            GuestMemoryErrorKind::Other,
                            FaultError::Protect(
                                MemoryRange::new(off as u64..(off + plen) as u64),
                                err,
                            ),
                        ));
                    }
                    // Prefetch the whole window in one call so the OS can back it
                    // with a contiguous large page. Best-effort: a failure
                    // (pre-Win11, or no large page available) just leaves 4 KB
                    // backing, so log and proceed. No first-fault claim here —
                    // leave the guest-side 2 MB widen to `resolve`; a later
                    // redundant prefetch over resident pages is harmless.
                    if let Err(err) = self.inner.mapping.prefetch(off, plen) {
                        sl.stats.prefetch_failures.increment();
                        tracing::debug!(
                            error = &err as &dyn std::error::Error,
                            address,
                            "soft large page prefetch failed"
                        );
                    } else {
                        sl.stats.host_prefetches.increment();
                    }
                    return PageFaultAction::Retry;
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

/// The soft-large-page granularity (2 MB) used for opportunistic large SLAT
/// entries and the per-region first-fault bitmap.
const LARGE_PAGE_SIZE: u64 = 2 * 1024 * 1024;

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

        // Widen to a 2 MB soft large page only on the first *write* fault of a
        // 2 MB window that both contains the whole fault and lies entirely within
        // one THP-eligible mapping. Read faults are served read-only from the
        // zero page and must not claim the window, so that the later first write
        // still gets its one widening attempt (spending a large page only on the
        // written working set). Every re-fault of an already-attempted region
        // (e.g. after the host trimmer reclaimed the pages) resolves to the
        // caller's range to avoid a hard-fault storm.
        let base = fault.start() & !(LARGE_PAGE_SIZE - 1);
        let window_end = base + LARGE_PAGE_SIZE;

        // Hold the mapping-index read lock across the widen decision and the
        // protect/prefetch syscalls below. This only blocks a concurrent
        // *writer* (a structural map/unmap), which is rare; other faulting VPs
        // are readers and proceed in parallel. `end` is the inclusive last
        // address of the mapping.
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
        let soft_lp = props.soft_lp.as_ref();

        let full_window = fault.end() <= window_end && base >= start && window_end <= end + 1;
        // Only a write fault claims the window (see the widen comment above); a
        // read fault leaves `first_fault` clear so the first write can still
        // widen.
        let widen = write
            && full_window
            && soft_lp.is_some_and(|sl| {
                sl.first_fault
                    .try_claim(base / LARGE_PAGE_SIZE - start / LARGE_PAGE_SIZE)
            });
        if widen {
            // `widen` implies `soft_lp` is `Some`.
            if let Some(sl) = soft_lp {
                sl.stats.guest_promotions.increment();
            }
        }

        // Deferred protect (Windows soft large pages): the range (private or
        // shared) is committed/mapped read-only, so a write fault must raise it
        // to read-write before the guest can proceed. Raise the whole 2 MB window
        // when it lies within the mapping and, on the window's first fault
        // (`widen`), prefetch it so the OS can back it with a large page;
        // otherwise raise just the faulting range. Read faults leave the range
        // read-only (served by the zero page), spending a large page only on the
        // written working set.
        #[cfg(windows)]
        if write {
            if let Some(sl) = soft_lp {
                let (prot_start, prot_end) = if full_window {
                    (base, window_end)
                } else {
                    // Clamp to the mapping (`end` is inclusive) so a fault range
                    // that spills past the region never protects an adjacent one.
                    (fault.start().max(start), fault.end().min(end + 1))
                };
                if let Err(err) = self.inner.mapping.protect(
                    prot_start as usize,
                    (prot_end - prot_start) as usize,
                    PAGE_READWRITE,
                ) {
                    return Err(GuestMemoryBackingError::new(
                        GuestMemoryErrorKind::Other,
                        fault.start(),
                        FaultError::Protect(MemoryRange::new(prot_start..prot_end), err),
                    ));
                }
                if widen {
                    // Prefetch the whole window in one call so the OS can back it
                    // with a contiguous large page. Best-effort: a failure
                    // (pre-Win11, or no large page available) just leaves 4 KB
                    // backing.
                    if let Err(err) = self
                        .inner
                        .mapping
                        .prefetch(base as usize, LARGE_PAGE_SIZE as usize)
                    {
                        sl.stats.prefetch_failures.increment();
                        tracing::debug!(
                            error = &err as &dyn std::error::Error,
                            gpa = fault.start(),
                            "soft large page prefetch failed"
                        );
                    }
                }
            }
        }

        Ok(if widen {
            MemoryRange::new(base..window_end)
        } else {
            fault
        })
    }
}

/// Number of aligned 2 MB regions spanned by `range` (by absolute 2 MB region
/// index), used to size a per-range [`FirstFault`] bitmap.
fn large_region_count(range: MemoryRange) -> u64 {
    (range.end() - 1) / LARGE_PAGE_SIZE - range.start() / LARGE_PAGE_SIZE + 1
}

/// Per-2 MB-region first-fault bitmap for a single THP-eligible mapping (Windows
/// soft large pages). A set bit means the region has already made its one
/// soft-large-page widening attempt; later faults resolve to a single page to
/// avoid a hard-fault storm.
///
/// Allocated per range and only for THP RAM on Windows, so guests without THP —
/// and every non-Windows guest — pay nothing.
#[derive(Debug)]
struct FirstFault(Box<[AtomicU64]>);

impl FirstFault {
    /// Allocates a bitmap covering `regions` 2 MB regions, all bits clear.
    fn new(regions: u64) -> Self {
        let words = regions.div_ceil(64);
        Self(
            (0..words)
                .map(|_| AtomicU64::new(0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    /// Allocates a bitmap covering `regions` 2 MB regions with every window
    /// already marked attempted. Used for prefetched ranges: the large pages
    /// are built eagerly at build time, so the lazy `resolve` path never
    /// re-widens a window. Bits past `regions` in the final word are set too,
    /// but are never indexed.
    fn new_all_claimed(regions: u64) -> Self {
        let words = regions.div_ceil(64);
        Self(
            (0..words)
                .map(|_| AtomicU64::new(!0))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        )
    }

    /// Atomically tests and sets the bit for `region`. Returns true if this
    /// call won the race (the bit was previously clear).
    fn try_claim(&self, region: u64) -> bool {
        let word = (region / 64) as usize;
        let bit = 1u64 << (region % 64);
        match self.0.get(word) {
            Some(slot) => slot.fetch_or(bit, Ordering::Relaxed) & bit == 0,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FirstFault;
    use super::large_region_count;
    use memory_range::MemoryRange;
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

    /// `try_claim` is one-shot: the first claim of a region wins and every
    /// subsequent claim of the same region loses until it is (never) cleared.
    #[test]
    fn first_fault_try_claim_is_one_shot() {
        let ff = FirstFault::new(4);
        // First claim of each region wins.
        for region in 0..4 {
            assert!(ff.try_claim(region), "first claim of region {region}");
        }
        // Repeat claims of the same regions all lose.
        for region in 0..4 {
            assert!(!ff.try_claim(region), "repeat claim of region {region}");
        }
    }

    /// Claims are independent across regions, including across the 64-bit word
    /// boundary of the underlying bitmap.
    #[test]
    fn first_fault_claims_are_independent() {
        // Enough regions to span more than one 64-bit word.
        let ff = FirstFault::new(130);
        assert!(ff.try_claim(0));
        assert!(ff.try_claim(63));
        assert!(ff.try_claim(64));
        assert!(ff.try_claim(129));
        // Claiming one region does not claim its neighbors.
        assert!(ff.try_claim(1));
        assert!(ff.try_claim(65));
        // But the already-claimed regions stay claimed.
        assert!(!ff.try_claim(0));
        assert!(!ff.try_claim(64));
    }

    /// Region indices past the end of the allocated bitmap return false rather
    /// than panicking, so a stray claim can never index out of bounds. (The
    /// bitmap rounds up to whole 64-bit words, so indices in the final word's
    /// padding are still claimable; callers only ever pass valid region
    /// indices.)
    #[test]
    fn first_fault_out_of_range_returns_false() {
        // One region rounds up to a single 64-bit word, so word 1 and beyond are
        // out of range.
        let ff = FirstFault::new(1);
        assert!(!ff.try_claim(64));
        assert!(!ff.try_claim(1_000_000));
    }

    /// `new_all_claimed` starts fully claimed, so no in-range window is ever
    /// widened (used for eagerly prefetched ranges).
    #[test]
    fn first_fault_new_all_claimed_prevents_widening() {
        let regions = 100;
        let ff = FirstFault::new_all_claimed(regions);
        for region in 0..regions {
            assert!(
                !ff.try_claim(region),
                "region {region} should already be claimed"
            );
        }
    }

    /// `large_region_count` counts the aligned 2 MB regions a range spans by
    /// absolute region index, so a range that straddles a 2 MB boundary spans
    /// two regions even when it is smaller than 2 MB.
    #[test]
    fn large_region_count_spans() {
        const LP: u64 = super::LARGE_PAGE_SIZE;
        // A single page at the start of a region spans one region.
        assert_eq!(large_region_count(MemoryRange::new(0..0x1000)), 1);
        // A full aligned region spans one region.
        assert_eq!(large_region_count(MemoryRange::new(0..LP)), 1);
        // A range straddling a 2 MB boundary spans two regions.
        assert_eq!(
            large_region_count(MemoryRange::new(LP - 0x1000..LP + 0x1000)),
            2
        );
        // Two full aligned regions span two regions.
        assert_eq!(large_region_count(MemoryRange::new(0..2 * LP)), 2);
    }
}
