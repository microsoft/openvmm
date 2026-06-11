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
//! In both modes, private memory ranges use commit-on-fault (Windows) or are
//! handled transparently by the kernel (Linux).

// UNSAFETY: Implementing the unsafe GuestMemoryAccess trait by calling unsafe
// low level memory manipulation functions.
#![expect(unsafe_code)]

use super::manager::DmaRegionProvider;
use super::manager::MapperId;
use super::manager::MapperRequest;
use super::manager::MappingError;
use super::manager::MappingParams;
use super::manager::MappingRequest;
use crate::RemoteProcess;
use futures::executor::block_on;
use guestmem::GuestMemoryAccess;
use guestmem::GuestMemorySharing;
use guestmem::PageFaultAction;
use guestmem::PageFaultError;
use memory_range::MemoryRange;
use mesh::error::RemoteError;
use mesh::rpc::RpcError;
use mesh::rpc::RpcSend;
use parking_lot::Mutex;
use sparse_mmap::SparseMapping;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use thiserror::Error;

#[derive(Debug, Error)]
#[error("unexpected page fault")]
struct UnexpectedPageFault;

/// A virtual address space mapper for guest memory.
///
/// Maintains a reserved VA range and maps file-backed or anonymous memory
/// into it as directed by the mapping manager.
pub struct VaMapper {
    inner: Arc<MapperInner>,
    id: MapperId,
    process: Option<RemoteProcess>,
    /// Ranges backed by private anonymous memory.
    /// Page faults in these ranges commit pages directly instead of
    /// requesting a file-backed mapping from the MappingManager.
    private_ranges: Vec<MemoryRange>,
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

#[derive(Debug)]
struct MapperInner {
    mapping: SparseMapping,
    /// Waiters for lazy mapping requests. `None` after the mapper task exits.
    waiters: Mutex<Option<Vec<MapWaiter>>>,
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
                }),
                MapperRequest::MapEager(rpc) => {
                    rpc.handle_failable_sync(|params| {
                        tracing::debug!(range = %params.range, "eager mapping received");
                        self.map_file(params)
                    });
                }
                MapperRequest::MapLazy(params) => {
                    tracing::debug!(range = %params.range, "lazy mapping received");
                    let (range, writable) = (params.range, params.writable);
                    match self.map_file(params) {
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

    /// Maps a file-backed region into the VA space, applying NUMA policy where
    /// supported.
    fn map_file(&self, params: MappingParams) -> Result<(), MappingError> {
        let MappingParams {
            range,
            mappable,
            writable,
            file_offset,
            numa_node,
            ..
        } = params;

        let map_result = cfg_select! {
            windows => {
                self.inner.mapping.map_file_numa(
                    range.start() as usize,
                    range.len() as usize,
                    &mappable,
                    file_offset,
                    writable,
                    numa_node,
                )
            }
            _ => {
                self.inner.mapping.map_file(
                    range.start() as usize,
                    range.len() as usize,
                    &mappable,
                    file_offset,
                    writable,
                )
            }
        };

        if let Err(e) = map_result {
            return Err(MappingError::new(range, e));
        }

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
                // NUMA handled by map_file_numa above.
                let _ = numa_node;
            }
            _ => {
                assert!(numa_node.is_none(), "NUMA not supported on this platform; should have been rejected at build time");
            }
        }

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
    #[error("remote mappers are not supported when any RAM backing uses private memory")]
    RemoteWithPrivateMemory,
}

/// Error returned when a lazy mapping request cannot be fulfilled.
#[derive(Debug, Error)]
#[error("no mapping for {0}")]
pub struct NoMapping(MemoryRange);

impl MapperInner {
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
}

impl VaMapper {
    pub(crate) async fn new(
        req_send: mesh::Sender<MappingRequest>,
        len: u64,
        remote_process: Option<RemoteProcess>,
        private_ranges: Vec<MemoryRange>,
        minimum_alignment: Option<usize>,
        eager: bool,
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
            eager: AtomicBool::new(eager),
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
            private_ranges,
            _thread: thread,
        })
    }

    /// Returns true if `addr` falls within a private range.
    fn is_private(&self, addr: u64) -> bool {
        self.private_ranges.iter().any(|r| r.contains_addr(addr))
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

    /// Allocates private anonymous memory for a range within the mapping,
    /// optionally bound to a specific host NUMA node.
    ///
    /// This replaces the placeholder at the given offset with committed
    /// anonymous memory.
    ///
    /// Caution: on Linux, if NUMA binding fails, the allocation itself has
    /// still succeeded — the returned error does not imply the memory is
    /// unmapped.
    pub(crate) fn alloc_range(
        &self,
        offset: usize,
        len: usize,
        numa_node: Option<u32>,
    ) -> Result<(), std::io::Error> {
        cfg_select! {
            windows => {
                self.inner.mapping.alloc_numa(offset, len, numa_node)
            }
            target_os = "linux" => {
                self.inner.mapping.alloc(offset, len)?;
                if let Some(node) = numa_node {
                    self.inner.mapping.mbind_at(offset, len, node)?;
                }
                Ok(())
            }
            _ => {
                assert!(numa_node.is_none(), "NUMA not supported on this platform; should have been rejected at build time");
                self.inner.mapping.alloc(offset, len)
            }
        }
    }

    /// Names a range within the mapping for debugging (visible in smaps).
    pub fn set_range_name(&self, offset: usize, len: usize, name: &str) {
        self.inner.mapping.set_name(offset, len, name);
    }

    /// Marks a range as eligible for Transparent Huge Pages.
    #[cfg(target_os = "linux")]
    pub(crate) fn madvise_hugepage(&self, offset: usize, len: usize) -> Result<(), std::io::Error> {
        self.inner.mapping.madvise_hugepage(offset, len)
    }

    /// Decommits a range of private RAM, releasing physical pages back to the
    /// host.
    ///
    /// The caller must ensure this is only called on ranges backed by
    /// private anonymous memory (allocated via [`alloc_range`](Self::alloc_range)).
    #[expect(dead_code)] // Will be used by ballooning / memory hot-remove.
    pub fn decommit(&self, offset: usize, len: usize) -> Result<(), std::io::Error> {
        assert!(
            self.private_ranges
                .iter()
                .any(|r| r.contains(&MemoryRange::new(offset as u64..offset as u64 + len as u64))),
            "decommit called on non-private range"
        );
        self.inner.mapping.decommit(offset, len)
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

        if self.is_private(address) {
            // Private RAM: commit the page(s) directly.
            #[cfg(windows)]
            {
                // Commit in 64KB-aligned chunks to amortize overhead.
                let commit_start = address & !0xFFFF; // round down to 64KB
                let commit_end = ((address + len as u64) + 0xFFFF) & !0xFFFF; // round up
                let commit_end = commit_end.min(self.inner.mapping.len() as u64);
                let commit_len = (commit_end - commit_start) as usize;

                if let Err(err) = self.inner.mapping.commit(commit_start as usize, commit_len) {
                    return PageFaultAction::Fail(PageFaultError::new(
                        guestmem::GuestMemoryErrorKind::Other,
                        err,
                    ));
                }
                return PageFaultAction::Retry;
            }
            #[cfg(unix)]
            {
                // On Linux, the kernel handles page faults transparently.
                // If we get here, something is wrong.
                return PageFaultAction::Fail(PageFaultError::new(
                    guestmem::GuestMemoryErrorKind::Other,
                    UnexpectedPageFault,
                ));
            }
        }

        if self.inner.eager.load(Ordering::Relaxed) {
            // Eager mapper: file-backed mappings are established proactively.
            // If we get a page fault, the mapping was never set up or was
            // torn down.
            return PageFaultAction::Fail(PageFaultError::new(
                guestmem::GuestMemoryErrorKind::OutOfRange,
                UnexpectedPageFault,
            ));
        }

        // Lazy mapper: request the mapping on demand from the mapping manager.
        let range = MemoryRange::bounding(address..address + len as u64);
        if let Err(err) = block_on(self.inner.request_mapping(self.id, range, write)) {
            return PageFaultAction::Fail(PageFaultError::new(
                guestmem::GuestMemoryErrorKind::OutOfRange,
                err,
            ));
        }
        PageFaultAction::Retry
    }

    fn sharing(&self) -> Option<GuestMemorySharing> {
        if !self.private_ranges.is_empty() {
            return None;
        }
        Some(GuestMemorySharing::new(DmaRegionProvider {
            req_send: self.inner.req_send.clone(),
        }))
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

    /// Tests that decommitting pages releases their contents (zeros on re-read on Linux).
    #[test]
    fn test_private_ram_decommit_zeros() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Commit and write data.
        mapping.alloc(0, 2 * page_size).unwrap();
        let pattern = vec![0xABu8; 64];
        mapping.write_at(0, &pattern).unwrap();
        mapping.write_at(page_size, &pattern).unwrap();

        // Decommit first page.
        mapping.decommit(0, page_size).unwrap();

        // On Linux, decommitted pages read as zeros.
        #[cfg(unix)]
        {
            let mut buf = vec![0xFFu8; 64];
            mapping.read_at(0, &mut buf).unwrap();
            assert!(
                buf.iter().all(|&b| b == 0),
                "decommitted page should be zeros on Linux"
            );
        }

        // Second page should still have its data.
        let mut buf2 = vec![0u8; 64];
        mapping.read_at(page_size, &mut buf2).unwrap();
        assert_eq!(buf2, pattern);
    }

    /// Tests that recommitting pages after decommit provides zeroed memory.
    #[test]
    fn test_private_ram_recommit_after_decommit() {
        let page_size = SparseMapping::page_size();
        let mapping = SparseMapping::new(4 * page_size).unwrap();

        // Commit, write, decommit, recommit.
        mapping.alloc(0, page_size).unwrap();
        let pattern = vec![0xCDu8; 64];
        mapping.write_at(0, &pattern).unwrap();

        mapping.decommit(0, page_size).unwrap();
        mapping.commit(0, page_size).unwrap();

        // After recommit, the page should be zeros (old data is gone).
        let mut buf = vec![0xFFu8; 64];
        mapping.read_at(0, &mut buf).unwrap();
        assert!(
            buf.iter().all(|&b| b == 0),
            "recommitted page should be zeros"
        );

        // Can write and read new data.
        let new_data = vec![0xEFu8; 64];
        mapping.write_at(0, &new_data).unwrap();
        let mut buf2 = vec![0u8; 64];
        mapping.read_at(0, &mut buf2).unwrap();
        assert_eq!(buf2, new_data);
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
