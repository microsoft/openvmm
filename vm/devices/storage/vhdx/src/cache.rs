// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Write-back page cache for VHDX metadata pages.
//!
//! Provides a hash-table-backed, page-granularity (4 KiB) caching layer over
//! an [`AsyncFile`](crate::AsyncFile). Pages are identified by a [`PageKey`]
//! consisting of a tag (u8) and an offset within a tagged region. Tags map
//! to base file offsets, allowing region relocation without invalidating
//! cached pages.
//!
//! Modified pages accumulate as **Dirty** in the cache. On [`commit()`](PageCache::commit),
//! dirty pages are sent to the [log task](crate::log_task) via a mesh channel
//! for WAL persistence. The log task applies them to their final file offsets
//! in the background.
//!
//! Page data is stored as `Arc<[u8; PAGE_SIZE]>` to enable zero-copy commit
//! (Arc::clone) and implicit COW (Arc::make_mut) when a page is modified while
//! the log task holds a reference.
//!
//! # Write Ordering
//!
//! The cache guarantees that writes are **ordered** through the log. If a
//! caller writes page A, then later writes page B, the only crash-recovery
//! outcomes are: {neither}, {A only}, or {both A and B}. It is never the case
//! that B is persisted without A.
//!
//! This ordering is maintained by **batch-full commit**: when the dirty page
//! count reaches [`MAX_COMMIT_PAGES`] and a new page is about to become dirty,
//! the cache automatically commits the current dirty set to the log before
//! allowing the new page to enter the dirty set.

use crate::AsyncFile;
use crate::error::CacheError;
use crate::flush::Fsn;
use crate::log_permits::LogPermits;
use crate::log_task::LogClient;
use crate::log_task::LogData;
use crate::log_task::Lsn;
use crate::lsn_watermark::LsnWatermark;
use parking_lot::ArcMutexGuard;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::collections::hash_map;
use std::sync::Arc;

/// Page size used by the cache (4 KiB).
pub const PAGE_SIZE: usize = 4096;

/// Maximum number of dirty pages per commit batch.
///
/// Derived from 1/4 of the minimum 1 MiB VHDX log. With 0 zero ranges:
///   entry_length(N) = ceil((64 + 32*N) / 4096) * 4096 + N * 4096
///   (N+1)*4096 + 4096 (guard) ≤ 262144  →  N ≤ 62
///
/// Note: the permit count is a *multiple* of this value (see `open.rs`)
/// to allow pipelining — multiple batches can be in-flight in the
/// log/apply pipeline simultaneously.
pub const MAX_COMMIT_PAGES: usize = 62;

/// Key identifying a cached page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageKey {
    /// Tag selecting the region (e.g., 0 = BAT, 1 = metadata).
    pub tag: u8,
    /// Byte offset within the tagged region. Must be 4 KiB aligned.
    pub offset: u64,
}

/// Write mode for page acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Page is loaded from file if not cached. Caller will modify parts.
    Modify,
    /// Page is NOT loaded from file (caller will overwrite the entire page).
    Overwrite,
}

/// Per-page lifecycle state.
///
/// Encodes the dirty flag, permit state, and data provenance as a single
/// enum to prevent invalid combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageState {
    /// Page is not dirty. Data may or may not be loaded (`data` can be
    /// `None` for a freshly created entry that hasn't been loaded or
    /// written yet).
    Clean,
    /// Page data is being loaded from disk by another task.
    /// Other acquirers wait on `state_event`.
    Loading,
    /// A log permit is being acquired for this page.
    /// Other acquirers wait on `state_event`.
    AcquiringPermit,
    /// Page has been modified (or a permit has been acquired for it).
    /// A permit is consumed (transfers to the log task on commit).
    Dirty,
}

/// Internal per-page data, generic over the file buffer type.
struct PageData<B> {
    /// The page contents as `Arc` for zero-copy commit and COW.
    /// `Some` when `state` is `Dirty`, and when `Clean` after a
    /// successful load or write.
    /// `None` when `Clean` (freshly created, not yet loaded),
    /// `Loading`, or `AcquiringPermit`.
    data: Option<Arc<B>>,
    /// Page lifecycle state.
    state: PageState,
    /// If set, the log task must wait for this FSN to complete before
    /// including this page in a log entry.
    pre_log_fsn: Option<Fsn>,
    /// Index into `PageMap::lru`. Allocated on entry creation.
    lru_index: usize,
    /// Hint: this page is cheap to regenerate; evict before other pages.
    demoted: bool,
}

/// Node in the slab-indexed LRU doubly-linked list.
struct LruNode {
    key: PageKey,
    prev: usize,
    next: usize,
    linked: bool,
}

/// Slab-indexed doubly-linked list for LRU eviction ordering.
///
/// Index 0 is a sentinel node. `sentinel.next` is the MRU end,
/// `sentinel.prev` is the LRU end (eviction candidate). All
/// operations are O(1).
struct LruList {
    nodes: Vec<LruNode>,
    free: Vec<usize>,
}

impl LruList {
    /// Create a new list with only the sentinel node.
    fn new() -> Self {
        Self {
            nodes: vec![LruNode {
                key: PageKey { tag: 0, offset: 0 },
                prev: 0,
                next: 0,
                linked: true, // sentinel is always "linked"
            }],
            free: Vec::new(),
        }
    }

    /// Allocate a slot for a new page. The node is NOT linked.
    fn alloc(&mut self, key: PageKey) -> usize {
        if let Some(idx) = self.free.pop() {
            self.nodes[idx] = LruNode {
                key,
                prev: 0,
                next: 0,
                linked: false,
            };
            idx
        } else {
            let idx = self.nodes.len();
            self.nodes.push(LruNode {
                key,
                prev: 0,
                next: 0,
                linked: false,
            });
            idx
        }
    }

    /// Returns true if the node is currently linked in the list.
    fn is_linked(&self, idx: usize) -> bool {
        debug_assert!(idx != 0, "cannot check sentinel linkage");
        self.nodes[idx].linked
    }

    /// Remove a node from its current position. No-op if unlinked.
    fn unlink(&mut self, idx: usize) {
        if idx == 0 || !self.is_linked(idx) {
            return;
        }
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        self.nodes[prev].next = next;
        self.nodes[next].prev = prev;
        self.nodes[idx].prev = 0;
        self.nodes[idx].next = 0;
        self.nodes[idx].linked = false;
    }

    /// Move a node to the MRU end. Works whether linked or unlinked.
    fn move_to_front(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        // Already at front?
        if self.nodes[0].next == idx {
            return;
        }
        // Remove from current position if linked.
        if self.nodes[idx].linked {
            let prev = self.nodes[idx].prev;
            let next = self.nodes[idx].next;
            self.nodes[prev].next = next;
            self.nodes[next].prev = prev;
        }
        // Insert after sentinel.
        let old_front = self.nodes[0].next;
        self.nodes[idx].prev = 0;
        self.nodes[idx].next = old_front;
        self.nodes[0].next = idx;
        self.nodes[old_front].prev = idx;
        self.nodes[idx].linked = true;
    }

    /// Move a node to the LRU end. Works whether linked or unlinked.
    fn move_to_back(&mut self, idx: usize) {
        if idx == 0 {
            return;
        }
        // Already at back?
        if self.nodes[0].prev == idx {
            return;
        }
        // Remove from current position if linked.
        if self.nodes[idx].linked {
            let prev = self.nodes[idx].prev;
            let next = self.nodes[idx].next;
            self.nodes[prev].next = next;
            self.nodes[next].prev = prev;
        }
        // Insert before sentinel.
        let old_back = self.nodes[0].prev;
        self.nodes[idx].next = 0;
        self.nodes[idx].prev = old_back;
        self.nodes[0].prev = idx;
        self.nodes[old_back].next = idx;
        self.nodes[idx].linked = true;
    }

    /// Return the index of the LRU tail node, or 0 if empty.
    fn tail(&self) -> usize {
        self.nodes[0].prev
    }

    /// Unlink and recycle a node.
    fn dealloc(&mut self, idx: usize) {
        self.unlink(idx);
        self.free.push(idx);
    }
}

/// Number of distinct cache tags (BAT=0, METADATA=1, SBM=2).
const TAG_COUNT: usize = 3;

/// Entry in the page map. Wraps the page data mutex with metadata
/// that can be read under the map lock without taking the page lock.
struct CacheEntry<B> {
    page: Arc<Mutex<PageData<B>>>,
    /// LSN of the most recent commit that included this page.
    /// Set under the map lock in `commit_locked`, read under the map
    /// lock in eviction — no page lock needed. 0 = never committed.
    committed_lsn: Lsn,
    /// True when the page is clean and no writer holds it.
    /// Set under the map lock on all write-acquire and write-release
    /// paths. Eviction checks this without taking the page lock.
    idle: bool,
}

/// Internal page map wrapping the `HashMap` and dirty page counter.
struct PageMap<B> {
    map: HashMap<PageKey, CacheEntry<B>>,
    /// Number of pages with a consumed permit (Dirty, or Clean with
    /// an active `WritePageGuard` that hasn't called `DerefMut` yet).
    /// Maintained under the map lock to prevent races.
    dirty_count: usize,
    /// Log client for sending transactions. `None` for read-only caches.
    log_client: Option<LogClient<B>>,
    /// Base file offset per tag, indexed by tag value.
    tag_offsets: [u64; TAG_COUNT],
    /// LRU eviction list. Protected by the same lock as `map`.
    lru: LruList,
}

impl<B> PageMap<B> {
    /// Resolve a [`PageKey`] to an absolute file offset.
    fn resolve_offset(&self, key: PageKey) -> u64 {
        self.tag_offsets[key.tag as usize] + key.offset
    }
}

/// Action to perform when a page isn't ready (returned by sync helpers).
/// This enum is `Send` — it never contains `ArcMutexGuard`.
enum PendingAction<B> {
    /// Wait for another task to finish loading/acquiring.
    Wait(event_listener::EventListener),
    /// Load page data from disk at this file offset. Carries the page
    /// entry Arc so `complete_load` can skip the map re-lookup.
    Load(u64, Arc<Mutex<PageData<B>>>),
}

/// Action for acquire_write when the page isn't ready.
/// This enum is `Send` — it never contains `ArcMutexGuard`.
enum WritePendingAction<B> {
    /// Wait for another task to finish loading/acquiring.
    Wait(event_listener::EventListener),
    /// Load page data from disk at this file offset. Carries the page
    /// entry Arc so `complete_load` can skip the map re-lookup.
    Load(u64, Arc<Mutex<PageData<B>>>),
    /// Acquire a log permit. Carries the page entry Arc so
    /// `finalize_permit` can skip the map re-lookup.
    AcquirePermit(Arc<Mutex<PageData<B>>>),
}

/// Log pipeline state shared between the cache and the log/apply tasks.
///
/// Present only when the file is opened writable with a log task.
pub(crate) struct CacheLogState {
    /// Failable semaphore for log backpressure.
    pub permits: Arc<LogPermits>,
    /// LSN watermark published by the apply task.
    pub applied_lsn: Arc<LsnWatermark>,
}

/// Write-back page cache backed by an [`AsyncFile`].
pub struct PageCache<F: AsyncFile> {
    pub(crate) file: Arc<F>,
    pages: Mutex<PageMap<F::Buffer>>,
    log_state: Option<CacheLogState>,
    /// Notified when a page transitions out of `Loading` or `AcquiringPermit`.
    state_event: event_listener::Event,
    /// Maximum number of pages to keep in the cache. 0 = unlimited.
    quota: usize,
}

impl<F: AsyncFile> PageCache<F> {
    /// Create a new cache backed by the given file.
    pub fn new(
        file: Arc<F>,
        log_client: Option<LogClient<F::Buffer>>,
        log_state: Option<CacheLogState>,
        quota: usize,
    ) -> Self {
        Self {
            file,
            pages: Mutex::new(PageMap {
                map: HashMap::new(),
                dirty_count: 0,
                log_client,
                tag_offsets: [0; TAG_COUNT],
                lru: LruList::new(),
            }),
            log_state,
            state_event: event_listener::Event::new(),
            quota,
        }
    }

    /// Take the log client out of the cache, returning it.
    pub fn take_log_client(&mut self) -> Option<LogClient<F::Buffer>> {
        self.pages.lock().log_client.take()
    }

    /// Set the log pipeline state (for late initialization after log task spawn).
    pub fn set_log_state(&mut self, state: CacheLogState) {
        self.log_state = Some(state);
    }

    /// Register a tag with its base file offset.
    pub fn register_tag(&mut self, tag: u8, base_offset: u64) {
        self.pages.lock().tag_offsets[tag as usize] = base_offset;
    }

    /// Evict clean, applied pages to bring the cache back under quota.
    /// Must be called with the pages map lock held.
    /// `skip_key` is the page being acquired — never evict it.
    fn try_evict_under_lock(&self, pages: &mut PageMap<F::Buffer>, skip_key: Option<PageKey>) {
        let applied = self
            .log_state
            .as_ref()
            .map(|s| s.applied_lsn.get())
            .unwrap_or(Lsn::ZERO);

        // Walk backward from the LRU tail. Check `idle` and
        // `committed_lsn` on the entry — both maintained under the
        // map lock, so no page lock needed.
        let mut idx = pages.lru.tail();
        while self.quota > 0 && pages.map.len() > self.quota {
            if idx == 0 {
                break;
            }
            let prev_idx = pages.lru.nodes[idx].prev;
            let key = pages.lru.nodes[idx].key;
            if skip_key == Some(key) {
                idx = prev_idx;
                continue;
            }
            let entry = pages.map.get(&key).expect("LRU key missing from map");
            if entry.idle && entry.committed_lsn <= applied {
                pages.map.remove(&key);
                pages.lru.dealloc(idx);
                idx = pages.lru.tail();
                continue;
            }
            idx = prev_idx;
        }
    }

    /// Acquire read access to a page.
    pub async fn acquire_read(&self, key: PageKey) -> Result<ReadPageGuard<F::Buffer>, CacheError> {
        loop {
            let action = match self.try_acquire_read(key) {
                Ok(guard) => return Ok(guard),
                Err(action) => action,
            };
            match action {
                PendingAction::Wait(listener) => listener.await,
                PendingAction::Load(file_offset, entry) => {
                    let buf = self.file.alloc_buffer(PAGE_SIZE);
                    match self.file.read_into(file_offset, buf).await {
                        Ok(buf) => self.complete_load(entry, Some(Arc::new(buf))),
                        Err(e) => {
                            self.complete_load(entry, None);
                            return Err(CacheError::Read {
                                err: e,
                                file_offset,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Sync helper: try to acquire read access.
    fn try_acquire_read(
        &self,
        key: PageKey,
    ) -> Result<ReadPageGuard<F::Buffer>, PendingAction<F::Buffer>> {
        assert!(
            key.offset.is_multiple_of(PAGE_SIZE as u64),
            "page offset {:#x} is not {PAGE_SIZE}-byte aligned",
            key.offset
        );
        let mut pages = self.pages.lock();
        let file_offset = pages.resolve_offset(key);

        // Pre-allocate an LRU slot. Freed below if the entry already exists.
        let lru_index = pages.lru.alloc(key);
        let mut inserted = false;
        let page = pages
            .map
            .entry(key)
            .or_insert_with(|| {
                inserted = true;
                CacheEntry {
                    page: Arc::new(Mutex::new(PageData {
                        data: None,
                        state: PageState::Clean,
                        pre_log_fsn: None,
                        lru_index,
                        demoted: false,
                    })),
                    committed_lsn: Lsn::ZERO,
                    idle: true,
                }
            })
            .page
            .clone();

        if !inserted {
            pages.lru.dealloc(lru_index);
        } else if self.quota > 0 && pages.map.len() > self.quota {
            self.try_evict_under_lock(&mut pages, Some(key));
        }

        let mut guard = Mutex::lock_arc(&page);

        match guard.state {
            PageState::Loading | PageState::AcquiringPermit => {
                let listener = self.state_event.listen();
                drop(guard);
                drop(pages);
                Err(PendingAction::Wait(listener))
            }
            PageState::Clean if guard.data.is_none() => {
                guard.state = PageState::Loading;
                let entry_arc = ArcMutexGuard::into_arc(guard);
                drop(pages);
                Err(PendingAction::Load(file_offset, entry_arc))
            }
            PageState::Clean | PageState::Dirty => {
                assert!(
                    guard.data.is_some(),
                    "page in {:?} has no data",
                    guard.state
                );
                // Promote to MRU.
                let idx = guard.lru_index;
                guard.demoted = false;
                pages.lru.move_to_front(idx);
                drop(pages);
                Ok(ReadPageGuard { guard })
            }
        }
    }

    /// Complete a page load: store data and transition out of Loading.
    ///
    /// On success (`data` is `Some`): stores data, transitions `Loading → Clean`.
    /// Uses the `entry` Arc directly — no map re-lookup needed.
    ///
    /// On failure (`data` is `None`): removes the entry from the cache so the
    /// next acquirer creates a fresh entry and retries.
    fn complete_load(&self, entry: Arc<Mutex<PageData<F::Buffer>>>, data: Option<Arc<F::Buffer>>) {
        let mut page = entry.lock();
        assert!(
            page.state == PageState::Loading,
            "complete_load called but page state is {:?}, expected Loading",
            page.state
        );
        assert!(
            page.data.is_none(),
            "complete_load called but page already has data"
        );
        page.state = PageState::Clean;
        page.data = data;
        self.state_event.notify(usize::MAX);
    }

    /// Acquire write access to a page.
    ///
    /// If a log is configured, acquires a permit (backpressure). If the
    /// dirty batch is full, commits it first (batch-full commit).
    pub async fn acquire_write(
        &self,
        key: PageKey,
        mode: WriteMode,
    ) -> Result<WritePageGuard<'_, F>, CacheError> {
        let load = mode == WriteMode::Modify;

        loop {
            let action = match self.try_acquire_write(key, load) {
                Ok(guard) => return Ok(guard),
                Err(action) => action,
            };
            match action {
                WritePendingAction::Wait(listener) => listener.await,
                WritePendingAction::Load(file_offset, entry) => {
                    let buf = self.file.alloc_buffer(PAGE_SIZE);
                    match self.file.read_into(file_offset, buf).await {
                        Ok(buf) => self.complete_load(entry, Some(Arc::new(buf))),
                        Err(e) => {
                            self.complete_load(entry, None);
                            return Err(CacheError::Read {
                                err: e,
                                file_offset,
                            });
                        }
                    }
                }
                WritePendingAction::AcquirePermit(entry) => {
                    let permits = &self.log_state.as_ref().unwrap().permits;
                    let result = permits.acquire(1).await;
                    match result {
                        Ok(()) => {
                            return self.finalize_permit(entry);
                        }
                        Err(e) => {
                            self.finalize_permit_failed(entry);
                            return Err(CacheError::PipelineFailed(e));
                        }
                    }
                }
            }
        }
    }

    /// Sync helper: try to acquire write access.
    ///
    /// Returns the guard on success, or an action to perform before
    /// retrying. Batch-full commit is handled in [`finalize_permit`].
    fn try_acquire_write(
        &self,
        key: PageKey,
        load: bool,
    ) -> Result<WritePageGuard<'_, F>, WritePendingAction<F::Buffer>> {
        assert!(
            self.log_state.is_some(),
            "acquire_write requires a log (use VhdxFile::open().writable())"
        );

        assert!(
            key.offset.is_multiple_of(PAGE_SIZE as u64),
            "page offset {:#x} is not {PAGE_SIZE}-byte aligned",
            key.offset
        );

        let file_offset;
        let mut pages = self.pages.lock();
        let mut guard = {
            let pages = &mut *pages;
            file_offset = pages.resolve_offset(key);

            // Pre-allocate an LRU slot. Freed below if the entry already exists.
            let lru_index = pages.lru.alloc(key);
            match pages.map.entry(key) {
                hash_map::Entry::Occupied(entry) => {
                    let entry = entry.into_mut();
                    pages.lru.dealloc(lru_index);
                    entry.idle = false;
                    entry.page.lock_arc()
                }
                hash_map::Entry::Vacant(entry) => {
                    let entry = entry.insert(CacheEntry {
                        page: Arc::new(Mutex::new(PageData {
                            data: None,
                            state: PageState::Clean,
                            pre_log_fsn: None,
                            lru_index,
                            demoted: false,
                        })),
                        committed_lsn: Lsn::ZERO,
                        idle: false,
                    });
                    let page = entry.page.clone();
                    if self.quota > 0 && pages.map.len() > self.quota {
                        self.try_evict_under_lock(pages, Some(key));
                    }
                    page.lock_arc()
                }
            }
        };

        match guard.state {
            PageState::Loading | PageState::AcquiringPermit => {
                Err(WritePendingAction::Wait(self.state_event.listen()))
            }
            PageState::Dirty => {
                assert!(
                    guard.data.is_some(),
                    "page in {:?} has no data",
                    guard.state
                );
                // Promote to MRU.
                let idx = guard.lru_index;
                guard.demoted = false;
                pages.lru.move_to_front(idx);
                drop(pages);
                Ok(WritePageGuard {
                    cache: self,
                    guard: Some(guard),
                    overwriting: false,
                })
            }
            PageState::Clean if load && guard.data.is_none() => {
                guard.state = PageState::Loading;
                Err(WritePendingAction::Load(
                    file_offset,
                    ArcMutexGuard::into_arc(guard),
                ))
            }
            PageState::Clean => {
                // Promote to MRU.
                let idx = guard.lru_index;
                guard.demoted = false;
                pages.lru.move_to_front(idx);
                guard.state = PageState::AcquiringPermit;
                Err(WritePendingAction::AcquirePermit(ArcMutexGuard::into_arc(
                    guard,
                )))
            }
        }
    }

    /// Finalize a successful permit acquisition.
    ///
    /// Returns the page guard directly — the caller wraps it in a
    /// `WritePageGuard` without re-entering `try_acquire_write`.
    /// This eliminates the window where the page is in HasPermit/Overwritten
    /// state without an active writer.
    ///
    /// The dirty_count check, batch-full commit, and dirty_count increment
    /// are all performed atomically under the map lock — no TOCTOU gap.
    fn finalize_permit(
        &self,
        entry: Arc<Mutex<PageData<F::Buffer>>>,
    ) -> Result<WritePageGuard<'_, F>, CacheError> {
        let mut pages = self.pages.lock();

        // Batch-full commit: if the dirty batch has reached
        // MAX_COMMIT_PAGES, commit before adding this page.
        if pages.dirty_count >= MAX_COMMIT_PAGES {
            if let Err(e) = self.commit_locked(&mut pages) {
                self.revert_permit(&entry, &mut pages);
                return Err(e);
            }
        }
        // Note that this may actually put us over MAX_COMMIT_PAGES, but only due to
        // transient dirty counts from pages that are clean and have not yet decremented
        // the count in [`WritePageGuard::drop`]. So, it will still be imposible for a
        // cache transaction to be larger than MAX_COMMIT_PAGES.
        pages.dirty_count += 1;

        let mut page = Mutex::lock_arc(&entry);
        assert!(page.state == PageState::AcquiringPermit);
        let overwriting = page.data.is_none();
        if overwriting {
            page.data = Some(Arc::new(self.file.alloc_buffer(PAGE_SIZE)));
        }
        // For overwriting pages (fresh zeros for Overwrite), set Dirty —
        // the zeros are synthetic, not real disk data, so they must be
        // committed even if the caller doesn't call DerefMut.
        page.state = if overwriting {
            PageState::Dirty
        } else {
            PageState::Clean
        };

        self.state_event.notify(usize::MAX);
        Ok(WritePageGuard {
            cache: self,
            guard: Some(page),
            overwriting,
        })
    }

    /// Release a clean write permit: mark idle, release the permit,
    /// decrement dirty_count, and notify waiters. The page must already
    /// be in `Clean` state.
    fn release_clean_permit(&self, lru_index: usize, pages: &mut PageMap<F::Buffer>) {
        pages.dirty_count -= 1;
        let key = pages.lru.nodes[lru_index].key;
        if let Some(ce) = pages.map.get_mut(&key) {
            ce.idle = true;
        }
        if let Some(ref state) = self.log_state {
            state.permits.release(1);
        }
        self.state_event.notify(usize::MAX);
    }

    /// Revert a page from `AcquiringPermit` back to `Clean` and release
    /// the permit. Used on permit acquisition failure and commit failure.
    fn revert_permit(
        &self,
        entry: &Arc<Mutex<PageData<F::Buffer>>>,
        pages: &mut PageMap<F::Buffer>,
    ) {
        let mut page = entry.lock();
        assert!(page.state == PageState::AcquiringPermit);
        page.state = PageState::Clean;
        let lru_index = page.lru_index;
        drop(page);
        self.release_clean_permit(lru_index, pages);
    }

    /// Finalize a failed permit acquisition: revert to Clean.
    fn finalize_permit_failed(&self, entry: Arc<Mutex<PageData<F::Buffer>>>) {
        let mut pages = self.pages.lock();
        self.revert_permit(&entry, &mut pages);
    }

    /// Get the pre-log FSN for a specific page, if set.
    #[cfg(test)]
    pub fn get_pre_log_fsn(&self, key: PageKey) -> Option<Fsn> {
        let pages = self.pages.lock();
        if let Some(entry) = pages.map.get(&key) {
            let page = entry.page.lock();
            page.pre_log_fsn
        } else {
            None
        }
    }

    /// Commit all dirty pages to the log task (fire-and-forget).
    ///
    /// Returns the current LSN. If there were dirty pages, they are sent
    /// to the log task and the returned LSN is the one assigned to that
    /// batch. If there were no dirty pages, returns the most recently
    /// assigned LSN (so that concurrent `flush()` callers still wait
    /// for any in-flight WAL writes).
    pub fn commit(&self) -> Result<Lsn, CacheError> {
        let mut pages = self.pages.lock();
        self.commit_locked(&mut pages)
    }

    /// Send pre-built page-aligned data through the log, bypassing the
    /// cache's dirty-page tracking. Used for non-cache metadata writes
    /// (e.g., region table repair).
    ///
    /// Returns the assigned LSN.
    pub fn commit_raw(&self, raw_pages: Vec<LogData<F::Buffer>>, pre_log_fsn: Option<Fsn>) -> Lsn {
        let mut map = self.pages.lock();
        let client = map
            .log_client
            .as_mut()
            .expect("commit_raw requires a log client (use VhdxFile::open().writable())");
        let txn = client.begin();
        txn.commit(raw_pages, pre_log_fsn)
    }

    /// Inner commit implementation that takes an already-held map lock.
    ///
    /// This allows `finalize_permit` to check dirty_count and commit
    /// atomically under the same lock — no TOCTOU gap.
    fn commit_locked(&self, pages: &mut PageMap<F::Buffer>) -> Result<Lsn, CacheError> {
        let client = pages
            .log_client
            .as_mut()
            .expect("commit requires a log client (use VhdxFile::open().writable())");

        let mut log_data = Vec::new();
        let mut max_pre_log_fsn: Option<Fsn> = None;

        let txn = client.begin();
        let lsn = txn.lsn();

        // Destructure to get separate borrows on map, lru, and tag_offsets.
        let PageMap {
            ref mut map,
            ref mut lru,
            ref tag_offsets,
            ..
        } = *pages;

        for (&key, entry) in map.iter_mut() {
            let mut page = entry.page.lock();
            if matches!(page.state, PageState::Dirty) {
                let file_offset = tag_offsets[key.tag as usize] + key.offset;
                let data = page.data.as_ref().expect("dirty page has no data").clone();

                if let Some(fsn) = page.pre_log_fsn.take() {
                    max_pre_log_fsn = Some(max_pre_log_fsn.map_or(fsn, |m| m.max(fsn)));
                }

                page.state = PageState::Clean;

                if page.demoted {
                    page.demoted = false;
                    lru.move_to_back(page.lru_index);
                }

                entry.committed_lsn = lsn;
                entry.idle = true;

                log_data.push(LogData::new(file_offset, data));
            }
        }

        if log_data.is_empty() {
            return Ok(client.current_lsn());
        }

        let committed_count = log_data.len();
        pages.dirty_count -= committed_count;

        txn.commit(log_data, max_pre_log_fsn);

        // Do NOT release permits here. Permits stay consumed until the
        // apply task writes pages to their final offsets and releases
        // them. This bounds the total in-flight page data (Arc clones)
        // in the log/apply pipeline, preventing unbounded memory growth.

        Ok(lsn)
    }
}

/// RAII guard providing read-only access to a cached page.
#[must_use = "page guard holds a lock; drop it when done reading"]
pub struct ReadPageGuard<B> {
    guard: ArcMutexGuard<parking_lot::RawMutex, PageData<B>>,
}

impl<B: AsRef<[u8]> + Send + Sync + 'static> std::ops::Deref for ReadPageGuard<B> {
    type Target = [u8; PAGE_SIZE];

    fn deref(&self) -> &[u8; PAGE_SIZE] {
        self.guard
            .data
            .as_ref()
            .expect("page data missing")
            .as_ref()
            .as_ref()
            .try_into()
            .expect("buffer is not PAGE_SIZE")
    }
}

/// RAII guard providing write access to a cached page.
///
/// Mutating via `DerefMut` transitions the page to `Dirty`. Arc COW
/// ensures the writer gets a private copy if the log task holds a
/// reference.
pub struct WritePageGuard<'a, F: AsyncFile> {
    cache: &'a PageCache<F>,
    guard: Option<ArcMutexGuard<parking_lot::RawMutex, PageData<F::Buffer>>>,
    /// Data existed before this acquire (loaded or previously written).
    /// False for first-touch Overwrite (zeroed data).
    overwriting: bool,
}

impl<F: AsyncFile> WritePageGuard<'_, F> {
    /// Returns true if the page is being overwritten rather than modified.
    ///
    /// If true, the page data is freshly zeroed and must be fully written by
    /// the caller (unless the caller just wants to commit a zero page).
    pub fn is_overwriting(&self) -> bool {
        self.overwriting
    }

    /// Set the pre-log flush sequence number on this page.
    pub fn set_pre_log_fsn(&mut self, fsn: Fsn) {
        let guard = self.guard.as_mut().expect("guard consumed");
        guard.pre_log_fsn = Some(match guard.pre_log_fsn {
            Some(existing) => existing.max(fsn),
            None => fsn,
        });
    }

    /// Hint that this page is cheap to regenerate and should be evicted
    /// before other pages (e.g., BAT pages that can be rebuilt from
    /// in-memory state).
    ///
    /// If the page becomes dirty, the demotion is applied when the page
    /// transitions back to clean in [`PageCache::commit`]. If the page
    /// stays clean (guard dropped without mutation), the demotion is
    /// applied immediately on drop.
    pub fn demote(&mut self) {
        self.guard.as_mut().expect("guard consumed").demoted = true;
    }
}

impl<F: AsyncFile> std::ops::Deref for WritePageGuard<'_, F> {
    type Target = [u8; PAGE_SIZE];

    fn deref(&self) -> &[u8; PAGE_SIZE] {
        self.guard
            .as_ref()
            .expect("guard consumed")
            .data
            .as_ref()
            .expect("page data missing")
            .as_ref()
            .as_ref()
            .try_into()
            .expect("buffer is not PAGE_SIZE")
    }
}

impl<F: AsyncFile> std::ops::DerefMut for WritePageGuard<'_, F> {
    fn deref_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        let guard = self.guard.as_mut().expect("guard consumed");
        guard.state = PageState::Dirty;
        let buf = Arc::make_mut(guard.data.as_mut().expect("page data missing"));
        buf.as_mut().try_into().expect("buffer is not PAGE_SIZE")
    }
}

impl<F: AsyncFile> Drop for WritePageGuard<'_, F> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            if guard.state != PageState::Dirty {
                // Guard dropped without mutation. Page is clean — release
                // the permit and mark idle.
                let lru_index = guard.lru_index;
                drop(guard);
                let mut pages = self.cache.pages.lock();
                self.cache.release_clean_permit(lru_index, &mut pages);
            }
            // If Dirty: permit consumed, page stays not-idle.
            // Guard drops, releasing page lock.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::log_task::LogRequest;
    use crate::tests::support::{FailingInterceptor, InMemoryFile};
    use pal_async::async_test;
    use std::sync::Arc;

    /// Helper to create a writable cache with log sender + permits.
    fn writable_cache(
        file: InMemoryFile,
    ) -> (PageCache<InMemoryFile>, mesh::Receiver<LogRequest<Vec<u8>>>) {
        let (tx, rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        (cache, rx)
    }

    #[async_test]
    async fn acquire_read_loads_from_file() {
        let file = InMemoryFile::new(PAGE_SIZE as u64);
        let pattern: Vec<u8> = (0..PAGE_SIZE).map(|i| (i & 0xFF) as u8).collect();
        file.write_at(0, &pattern).await.unwrap();

        let mut cache = PageCache::new(Arc::new(file), None, None, 0);
        cache.register_tag(0, 0);

        let guard = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(&guard[..], &pattern[..]);
    }

    #[async_test]
    async fn acquire_modify_loads_and_writes_back() {
        let file = InMemoryFile::new(PAGE_SIZE as u64);
        let pattern: Vec<u8> = (0..PAGE_SIZE).map(|i| (i & 0xFF) as u8).collect();
        file.write_at(0, &pattern).await.unwrap();

        let (_cache, _rx) = writable_cache(InMemoryFile::new(PAGE_SIZE as u64));
        // Re-create with the patterned file.
        let file = InMemoryFile::new(PAGE_SIZE as u64);
        file.write_at(0, &pattern).await.unwrap();
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        {
            let mut guard = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Modify)
                .await
                .unwrap();
            assert_eq!(guard[0], 0x00);
            assert_eq!(guard[1], 0x01);
            guard[0] = 0xAA;
            guard[1] = 0xBB;
        }

        let guard = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(guard[0], 0xAA);
        assert_eq!(guard[1], 0xBB);
        assert_eq!(guard[2], 0x02);
    }

    #[async_test]
    async fn acquire_overwrite_skips_read() {
        let file = InMemoryFile::with_interceptor(
            PAGE_SIZE as u64,
            Arc::new(FailingInterceptor {
                fail_reads: true,
                fail_writes: false,
                fail_flushes: false,
                fail_set_file_size: false,
            }),
        );

        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        {
            let mut guard = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Overwrite)
                .await
                .unwrap();
            guard.fill(0xCC);
        }

        let guard = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert!(guard.iter().all(|&b| b == 0xCC));
    }

    #[async_test]
    async fn concurrent_reads_return_correct_data() {
        let file = InMemoryFile::new(PAGE_SIZE as u64);
        let pattern: Vec<u8> = (0..PAGE_SIZE).map(|i| ((i * 3) & 0xFF) as u8).collect();
        file.write_at(0, &pattern).await.unwrap();

        let mut cache = PageCache::new(Arc::new(file), None, None, 0);
        cache.register_tag(0, 0);

        let g1 = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(&g1[..], &pattern[..]);
        drop(g1);

        let g2 = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(&g2[..], &pattern[..]);
    }

    #[async_test]
    async fn sequential_modify_acquires_work() {
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        {
            let mut guard = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Modify)
                .await
                .unwrap();
            guard[0] = 0x11;
        }

        {
            let mut guard = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Modify)
                .await
                .unwrap();
            assert_eq!(guard[0], 0x11);
            guard[0] = 0x22;
        }

        let guard = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(guard[0], 0x22);
    }

    #[async_test]
    async fn modify_then_modify_same_page() {
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        {
            let mut g = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Modify)
                .await
                .unwrap();
            g[0] = 0xAA;
        }

        {
            let mut g = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Modify)
                .await
                .unwrap();
            assert_eq!(g[0], 0xAA);
            g[1] = 0xBB;
        }

        let guard = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(guard[0], 0xAA);
        assert_eq!(guard[1], 0xBB);
    }

    #[async_test]
    async fn different_pages_independent() {
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64 * 4)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        {
            let mut g = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Modify)
                .await
                .unwrap();
            g[0] = 0x11;
        }

        {
            let mut g = cache
                .acquire_write(
                    PageKey {
                        tag: 0,
                        offset: PAGE_SIZE as u64,
                    },
                    WriteMode::Modify,
                )
                .await
                .unwrap();
            g[0] = 0x22;
        }

        let g1 = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(g1[0], 0x11);
        drop(g1);

        let g2 = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64,
            })
            .await
            .unwrap();
        assert_eq!(g2[0], 0x22);
    }

    #[async_test]
    async fn tag_offset_resolution() {
        let base: u64 = 0x10000;
        let page_offset: u64 = 0x1000;
        let file = InMemoryFile::new(base + page_offset + PAGE_SIZE as u64);
        let pattern = [0xDE; PAGE_SIZE];
        file.write_at(base + page_offset, &pattern).await.unwrap();

        let mut cache = PageCache::new(Arc::new(file), None, None, 0);
        cache.register_tag(0, base);

        let guard = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: page_offset,
            })
            .await
            .unwrap();
        assert_eq!(&guard[..], &pattern[..]);
    }

    #[async_test]
    async fn commit_sends_transaction() {
        let (tx, mut rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);
        let key = PageKey { tag: 0, offset: 0 };

        {
            let mut g = cache.acquire_write(key, WriteMode::Modify).await.unwrap();
            g.fill(0xAA);
        }
        let lsn = cache.commit().unwrap();
        assert!(lsn > Lsn::ZERO);

        match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => {
                assert_eq!(txn.lsn, lsn);
                assert_eq!(txn.data.len(), 1);
                assert!(txn.data[0].data().iter().all(|&b| b == 0xAA));
            }
            _ => panic!("expected Commit"),
        }
    }

    #[async_test]
    async fn consecutive_commits_get_increasing_lsns() {
        let (tx, mut rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);
        let key = PageKey { tag: 0, offset: 0 };

        {
            let mut g = cache.acquire_write(key, WriteMode::Modify).await.unwrap();
            g.fill(0xAA);
        }
        let lsn1 = cache.commit().unwrap();

        {
            let mut g = cache.acquire_write(key, WriteMode::Modify).await.unwrap();
            g.fill(0xBB);
        }
        let lsn2 = cache.commit().unwrap();

        assert!(lsn2 > lsn1);

        match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => assert_eq!(txn.lsn, lsn1),
            _ => panic!("expected Commit"),
        }
        match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => assert_eq!(txn.lsn, lsn2),
            _ => panic!("expected Commit"),
        }
    }

    #[async_test]
    async fn commit_sets_committed_lsn() {
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);
        let key = PageKey { tag: 0, offset: 0 };

        {
            let mut g = cache.acquire_write(key, WriteMode::Modify).await.unwrap();
            g.fill(0xAA);
        }
        let lsn = cache.commit().unwrap();

        let pages = cache.pages.lock();
        let entry = pages.map.get(&key).unwrap();
        assert_eq!(entry.committed_lsn, lsn);
    }

    async fn dirty_pages<F: AsyncFile>(cache: &PageCache<F>, count: usize) {
        for i in 0..count {
            let key = PageKey {
                tag: 0,
                offset: (i * PAGE_SIZE) as u64,
            };
            let mut g = cache
                .acquire_write(key, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(i as u8);
        }
    }

    #[async_test]
    async fn batch_full_commit_on_dirty_overflow() {
        let (tx, mut rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64 * 200)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        dirty_pages(&cache, MAX_COMMIT_PAGES).await;

        let new_key = PageKey {
            tag: 0,
            offset: (MAX_COMMIT_PAGES * PAGE_SIZE) as u64,
        };
        {
            let mut guard = cache
                .acquire_write(new_key, WriteMode::Overwrite)
                .await
                .unwrap();
            guard.fill(0xFF);
        }

        match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => {
                assert_eq!(txn.data.len(), MAX_COMMIT_PAGES);
            }
            _ => panic!("expected Commit from batch-full commit"),
        }

        cache.commit().unwrap();
        match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => {
                assert_eq!(txn.data.len(), 1);
            }
            _ => panic!("expected Commit from explicit commit"),
        }
    }

    #[async_test]
    async fn redirty_does_not_trigger_batch_full_commit() {
        let (tx, mut rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64 * 200)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        dirty_pages(&cache, MAX_COMMIT_PAGES).await;

        let key = PageKey { tag: 0, offset: 0 };
        let mut g = cache.acquire_write(key, WriteMode::Modify).await.unwrap();
        g[0] = 0xDD;

        assert!(
            rx.try_recv().is_err(),
            "re-dirtying an already-dirty page must not trigger batch-full commit"
        );

        assert_eq!(cache.pages.lock().dirty_count, MAX_COMMIT_PAGES);
    }

    #[async_test]
    async fn write_ordering_across_batches() {
        let (tx, mut rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64 * 200)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        dirty_pages(&cache, MAX_COMMIT_PAGES).await;

        let key_b = PageKey {
            tag: 0,
            offset: (MAX_COMMIT_PAGES * PAGE_SIZE) as u64,
        };
        {
            let mut g = cache
                .acquire_write(key_b, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(0xBB);
        }

        let batch1 = match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => txn,
            _ => panic!("expected Commit"),
        };
        assert_eq!(batch1.data.len(), MAX_COMMIT_PAGES);

        let key_c = PageKey {
            tag: 0,
            offset: ((MAX_COMMIT_PAGES + 1) * PAGE_SIZE) as u64,
        };
        {
            let mut g = cache
                .acquire_write(key_c, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(0xCC);
        }

        cache.commit().unwrap();
        let batch2 = match rx.recv().await.unwrap() {
            LogRequest::Commit(txn) => txn,
            _ => panic!("expected Commit"),
        };
        assert_eq!(batch2.data.len(), 2);
        assert!(batch1.lsn < batch2.lsn);
    }

    // ---- Eviction tests ----

    #[async_test]
    async fn eviction_removes_clean_page() {
        let file = InMemoryFile::new(PAGE_SIZE as u64 * 4);
        let pattern_a = [0xAA; PAGE_SIZE];
        let pattern_b = [0xBB; PAGE_SIZE];
        file.write_at(0, &pattern_a).await.unwrap();
        file.write_at(PAGE_SIZE as u64, &pattern_b).await.unwrap();

        // Quota of 1 page.
        let mut cache = PageCache::new(Arc::new(file), None, None, 1);
        cache.register_tag(0, 0);

        // Load page A.
        let g = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(g[0], 0xAA);
        drop(g);

        // Cache has 1 page (at quota). Loading page B should evict page A.
        let g = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64,
            })
            .await
            .unwrap();
        assert_eq!(g[0], 0xBB);
        drop(g);

        // Page A was evicted — cache should have 1 entry.
        assert_eq!(cache.pages.lock().map.len(), 1);
    }

    #[async_test]
    async fn eviction_reloads_from_disk() {
        let file = InMemoryFile::new(PAGE_SIZE as u64 * 4);
        let pattern_a = [0xAA; PAGE_SIZE];
        let pattern_b = [0xBB; PAGE_SIZE];
        file.write_at(0, &pattern_a).await.unwrap();
        file.write_at(PAGE_SIZE as u64, &pattern_b).await.unwrap();

        let mut cache = PageCache::new(Arc::new(file), None, None, 1);
        cache.register_tag(0, 0);

        // Load page A.
        let g = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(g[0], 0xAA);
        drop(g);

        // Load page B (evicts A).
        let g = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64,
            })
            .await
            .unwrap();
        assert_eq!(g[0], 0xBB);
        drop(g);

        // Re-load page A (evicts B, reloads from disk).
        let g = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(g[0], 0xAA);
        drop(g);
    }

    #[async_test]
    async fn eviction_skips_dirty_pages() {
        let file = InMemoryFile::new(PAGE_SIZE as u64 * 4);
        file.write_at(PAGE_SIZE as u64, &[0xBB; PAGE_SIZE])
            .await
            .unwrap();

        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        // Quota of 1, but page 0 will be dirty.
        let mut cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            1,
        );
        cache.register_tag(0, 0);

        // Write page A (makes it Dirty).
        {
            let mut g = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(0xAA);
        }

        // Try to load page B. Eviction should skip dirty page A.
        // Cache will have 2 entries (over quota but nothing evictable).
        let g = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64,
            })
            .await
            .unwrap();
        assert_eq!(g[0], 0xBB);
        drop(g);

        // Both pages present.
        assert_eq!(cache.pages.lock().map.len(), 2);

        // Verify page A is still readable (not evicted).
        let g = cache
            .acquire_read(PageKey { tag: 0, offset: 0 })
            .await
            .unwrap();
        assert_eq!(g[0], 0xAA);
    }

    #[async_test]
    async fn eviction_skips_uncommitted_page() {
        let file = InMemoryFile::new(PAGE_SIZE as u64 * 4);
        file.write_at(0, &[0xAA; PAGE_SIZE]).await.unwrap();
        file.write_at(PAGE_SIZE as u64, &[0xBB; PAGE_SIZE])
            .await
            .unwrap();

        let applied = Arc::new(LsnWatermark::new());
        // applied_lsn = 0, so committed pages with lsn > 0 are not evictable.

        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: applied.clone(),
            }),
            1,
        );
        cache.register_tag(0, 0);

        // Write and commit page A (committed_lsn = 1, applied_lsn = 0).
        {
            let mut g = cache
                .acquire_write(PageKey { tag: 0, offset: 0 }, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(0xAA);
        }
        cache.commit().unwrap();

        // Page A is Clean with committed_lsn=1. applied_lsn=0.
        // Eviction should skip it (not yet applied).
        let g = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64,
            })
            .await
            .unwrap();
        assert_eq!(g[0], 0xBB);
        drop(g);

        // Both pages present (A is not evictable).
        assert_eq!(cache.pages.lock().map.len(), 2);

        // Now advance applied_lsn past the committed_lsn.
        applied.advance(Lsn::new(1), Fsn::ZERO);

        // Load another page — now A is evictable.
        let _file_size = PAGE_SIZE as u64 * 4;
        // Load page at offset 2*PAGE_SIZE (need data there).
        cache
            .file
            .write_at(PAGE_SIZE as u64 * 2, &[0xCC; PAGE_SIZE])
            .await
            .unwrap();
        let g = cache
            .acquire_read(PageKey {
                tag: 0,
                offset: PAGE_SIZE as u64 * 2,
            })
            .await
            .unwrap();
        assert_eq!(g[0], 0xCC);
        drop(g);

        // Should have evicted one of the old pages (A or B).
        assert!(cache.pages.lock().map.len() <= 2);
    }

    #[async_test]
    async fn no_deadlock_with_quota() {
        // Regression test: verify that acquiring pages with a small quota
        // doesn't deadlock. The dual-lock pattern
        // should prevent lock-order issues.
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64 * 10)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            2,
        );
        cache.register_tag(0, 0);

        // Rapidly acquire and drop pages, cycling through more than the quota.
        for i in 0..5u64 {
            let mut g = cache
                .acquire_write(
                    PageKey {
                        tag: 0,
                        offset: i * PAGE_SIZE as u64,
                    },
                    WriteMode::Overwrite,
                )
                .await
                .unwrap();
            g.fill(i as u8);
        }
        // If we get here without hanging, no deadlock.
    }

    #[async_test]
    async fn overwrite_uncached_reports_not_cached() {
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        let key = PageKey { tag: 0, offset: 0 };
        let g = cache
            .acquire_write(key, WriteMode::Overwrite)
            .await
            .unwrap();
        assert!(
            g.is_overwriting(),
            "first Overwrite acquire should report overwriting (not cached)"
        );
    }

    #[async_test]
    async fn overwrite_cached_reports_cached() {
        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(InMemoryFile::new(PAGE_SIZE as u64)),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        let key = PageKey { tag: 0, offset: 0 };

        // First write populates the cache.
        {
            let mut g = cache
                .acquire_write(key, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(0xAA);
        }

        // Second write should find it cached.
        let g = cache
            .acquire_write(key, WriteMode::Overwrite)
            .await
            .unwrap();
        assert!(
            !g.is_overwriting(),
            "second Overwrite acquire should report cached (not overwriting)"
        );
        assert_eq!(g[0], 0xAA);
        assert_eq!(g[PAGE_SIZE - 1], 0xAA);
    }

    #[async_test]
    async fn modify_always_reports_cached() {
        // Modify loads from disk if not cached, so populated reflects
        // map presence after load — always true since load populates it.
        let file = InMemoryFile::new(PAGE_SIZE as u64);
        file.write_at(0, &[0xBB; PAGE_SIZE]).await.unwrap();

        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let mut cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: Arc::new(LsnWatermark::new()),
            }),
            0,
        );
        cache.register_tag(0, 0);

        let key = PageKey { tag: 0, offset: 0 };

        // Modify loads from disk then retries — page is in map on retry.
        let g = cache.acquire_write(key, WriteMode::Modify).await.unwrap();
        assert!(
            !g.is_overwriting(),
            "Modify always reports cached (not overwriting)"
        );
        assert_eq!(g[0], 0xBB);
    }

    #[async_test]
    async fn lru_evicts_oldest_first() {
        let file = InMemoryFile::new(PAGE_SIZE as u64 * 4);
        for i in 0..4 {
            file.write_at(i * PAGE_SIZE as u64, &[(i as u8) + 0xA0; PAGE_SIZE])
                .await
                .unwrap();
        }

        // Quota of 2.
        let mut cache = PageCache::new(Arc::new(file), None, None, 2);
        cache.register_tag(0, 0);

        let key_a = PageKey { tag: 0, offset: 0 };
        let key_b = PageKey {
            tag: 0,
            offset: PAGE_SIZE as u64,
        };
        let key_c = PageKey {
            tag: 0,
            offset: 2 * PAGE_SIZE as u64,
        };

        // Load A then B (both in cache, at quota).
        let g = cache.acquire_read(key_a).await.unwrap();
        assert_eq!(g[0], 0xA0);
        drop(g);

        let g = cache.acquire_read(key_b).await.unwrap();
        assert_eq!(g[0], 0xA1);
        drop(g);

        // LRU order: MRU=B, LRU=A. Loading C should evict A.
        let g = cache.acquire_read(key_c).await.unwrap();
        assert_eq!(g[0], 0xA2);
        drop(g);

        let pages = cache.pages.lock();
        assert!(
            !pages.map.contains_key(&key_a),
            "A should have been evicted"
        );
        assert!(pages.map.contains_key(&key_b), "B should still be cached");
        assert!(pages.map.contains_key(&key_c), "C should be cached");
    }

    #[async_test]
    async fn write_demote_defers_to_commit() {
        let file = InMemoryFile::new(PAGE_SIZE as u64 * 4);
        for i in 0..3 {
            file.write_at(i * PAGE_SIZE as u64, &[(i as u8) + 0xC0; PAGE_SIZE])
                .await
                .unwrap();
        }

        let (tx, _rx) = mesh::channel::<LogRequest<Vec<u8>>>();
        let permits = Arc::new(LogPermits::new(1000));
        let applied = Arc::new(LsnWatermark::new());
        let mut cache = PageCache::new(
            Arc::new(file),
            Some(LogClient::new(tx)),
            Some(CacheLogState {
                permits,
                applied_lsn: applied.clone(),
            }),
            2,
        );
        cache.register_tag(0, 0);

        let key_a = PageKey { tag: 0, offset: 0 };
        let key_b = PageKey {
            tag: 0,
            offset: PAGE_SIZE as u64,
        };
        let key_c = PageKey {
            tag: 0,
            offset: 2 * PAGE_SIZE as u64,
        };

        // Write A with demote. Page becomes dirty.
        {
            let mut g = cache
                .acquire_write(key_a, WriteMode::Overwrite)
                .await
                .unwrap();
            g.fill(0xDD);
            g.demote();
        }

        // Read B.
        let g = cache.acquire_read(key_b).await.unwrap();
        drop(g);

        // Commit A (dirty→clean). Since demoted, it should go to LRU end.
        let lsn = cache.commit().unwrap();
        applied.advance(lsn, Fsn::ZERO);

        // Now load C. Should evict A (demoted at LRU end) not B.
        let g = cache.acquire_read(key_c).await.unwrap();
        assert_eq!(g[0], 0xC2);
        drop(g);

        let pages = cache.pages.lock();
        assert!(
            !pages.map.contains_key(&key_a),
            "demoted A should be evicted after commit"
        );
        assert!(pages.map.contains_key(&key_b), "B should still be cached");
    }
}
