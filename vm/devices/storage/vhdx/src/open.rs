// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX file open orchestration.
//!
//! Ties together header, region, metadata, and BAT parsing into
//! [`VhdxFile::open()`], which returns a [`VhdxBuilder`] for
//! configuring options before finalizing as read-only or writable.

use crate::AsyncFile;
use crate::bat::BAT_TAG;
use crate::bat::Bat;
use crate::cache::PageCache;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::error::OpenErrorInner;
use crate::error::PipelineFailed;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::flush::FlushSequencer;
use crate::format;
use crate::format::FileIdentifier;
use crate::header::HeaderState;
use crate::header::WriteMode;
use crate::header::parse_headers;
use crate::header::serialize_header;
use crate::known_meta::read_known_metadata;
use crate::known_meta::verify_known_metadata;
use crate::log;
use crate::log::LogRegion;
use crate::log_task::LogRequest;
use crate::metadata::METADATA_TAG;
use crate::metadata::MetadataTable;
use crate::region::parse_region_tables;
use crate::sector_bitmap::SBM_TAG;
use crate::space::DeferredReleases;
use crate::space::EofState;
use crate::space::FreeSpaceTracker;
use guid::Guid;
use parking_lot::Mutex;
use std::sync::Arc;
use zerocopy::FromBytes;

/// Builder for opening a VHDX file.
///
/// Created via [`VhdxFile::open()`], then configured with builder methods
/// before calling [`read_only()`](Self::read_only) or
/// [`writable()`](Self::writable) to produce a [`VhdxFile`].
///
/// # Examples
///
/// ```ignore
/// // Default options:
/// let vhdx = VhdxFile::open(file).read_only().await?;
///
/// // With block alignment (for NTFS-DAX / PMEM volumes):
/// let vhdx = VhdxFile::open(file)
///     .block_alignment(2 * 1024 * 1024)
///     .writable(&spawner)
///     .await?;
/// ```
pub struct VhdxBuilder<F> {
    file: F,
    options: OpenOptions,
}

/// Internal options collected by [`VhdxBuilder`].
#[derive(Debug, Clone)]
struct OpenOptions {
    /// Block data alignment in bytes. Must be 0 or a power of 2.
    ///
    /// When non-zero and ≤ the VHDX block size, new data block allocations
    /// from the end of the file are rounded up to this alignment. This
    /// matches the host filesystem's cluster size (e.g. 2 MiB on NTFS-DAX
    /// volumes) so that data blocks land on cluster boundaries.
    ///
    /// Default: 0 (no alignment — blocks use the natural 1 MiB granularity).
    block_alignment: u32,
    /// Whether to allow log replay on a read-only open.
    ///
    /// When true, a dirty log is replayed (the file handle must support
    /// writes for the replay I/O) but the resulting `VhdxFile` is still
    /// read-only. When false, a dirty log returns
    /// [`CorruptionType::LogReplayRequired`].
    ///
    /// Ignored for writable opens (log replay always happens).
    ///
    /// Default: false.
    allow_replay: bool,
}

impl OpenOptions {
    fn new() -> Self {
        Self {
            block_alignment: 0,
            allow_replay: false,
        }
    }
}

impl<F: 'static + AsyncFile> VhdxBuilder<F> {
    /// Set the block data alignment in bytes.
    ///
    /// Must be 0 or a power of 2. If larger than the VHDX block size,
    /// it is silently ignored at open time.
    ///
    /// This should be set to the host filesystem's cluster size when the
    /// VHDX file lives on a volume with clusters larger than 1 MiB (e.g.
    /// NTFS-DAX with 2 MiB clusters).
    pub fn block_alignment(mut self, alignment: u32) -> Self {
        self.options.block_alignment = alignment;
        self
    }

    /// Allow log replay when opening read-only.
    ///
    /// When true, a dirty log is replayed (the file handle must support
    /// writes for the replay I/O) but the resulting [`VhdxFile`] is still
    /// read-only. When false, a dirty log returns an error.
    ///
    /// Has no effect on [`writable()`](Self::writable) opens, which always
    /// replay.
    pub fn allow_replay(mut self, allow: bool) -> Self {
        self.options.allow_replay = allow;
        self
    }

    /// Open the VHDX file in read-only mode.
    pub async fn read_only(self) -> Result<VhdxFile<F>, OpenError>
    where
        F: AsyncFile,
    {
        VhdxFile::open_read_only(self.file, &self.options).await
    }

    /// Open the VHDX file in writable mode with a log task.
    ///
    /// Replays a dirty log if needed, then spawns a log task for
    /// crash-consistent metadata writes.
    ///
    /// Call [`VhdxFile::close()`] for a clean shutdown.
    pub async fn writable(
        self,
        spawner: &impl pal_async::task::Spawn,
    ) -> Result<VhdxFile<F>, OpenError>
    where
        F: AsyncFile,
    {
        VhdxFile::open_writable(self.file, spawner, &self.options).await
    }
}

/// An open VHDX file handle.
///
/// Created via [`VhdxFile::open()`], which returns a [`VhdxBuilder`]
/// for configuring options before calling
/// [`read_only()`](VhdxBuilder::read_only) or
/// [`writable()`](VhdxBuilder::writable).
//
// Lock ordering (must acquire in this order, never reverse):
//   1. header_state.inner  (futures::lock::Mutex — async, may be held across .await)
//   2. allocation_lock     (futures::lock::Mutex — async, may be held across .await)
//   3. bat_state            (parking_lot::RwLock — synchronous, NEVER across .await)
//   4. free_space.inner     (parking_lot::Mutex — synchronous, NEVER across .await)
//   5. cache.pages/tags     (parking_lot::Mutex — brief, NEVER across .await)
//
// header_state.inner serializes all header writes (enable_write_mode, set_log_guid,
// clear_log_guid). Its write_mode AtomicU8 provides a lock-free fast path for
// enable_write_mode, which is called on every write.
// The allocation_lock serializes the entire allocation decision (check BAT, allocate
// space, mark TFP). It is released AFTER TFP is set but BEFORE data I/O begins.
// The bat_state RwLock is held for < 1μs per access (reading/writing in-memory entries).
pub struct VhdxFile<F: AsyncFile> {
    pub(crate) file: Arc<F>,
    pub(crate) cache: PageCache<F>,
    pub(crate) bat: Bat,

    // Parsed metadata
    pub(crate) disk_size: u64,
    pub(crate) block_size: u32,
    pub(crate) logical_sector_size: u32,
    physical_sector_size: u32,
    pub(crate) has_parent: bool,
    is_fully_allocated: bool,
    page_83_data: Guid,

    // Metadata table (kept for on-demand metadata reads).
    metadata_table: MetadataTable,

    // Header and write-mode state (async mutex for serialization,
    // AtomicU8 for lock-free hot-path write-mode checks).
    pub(crate) header_state: HeaderState,

    /// Serializes block allocation decisions and protects EOF geometry
    /// state. Only one allocation sequence runs at a time.
    /// Uses futures::lock::Mutex because it may be held across .await points.
    pub(crate) allocation_lock: futures::lock::Mutex<EofState>,

    /// Broadcast event notified when a TFP block completes post-allocation.
    /// Writers that encounter a TFP block listen on this event and retry.
    pub(crate) allocation_event: event_listener::Event,

    /// Free space tracker. Manages all space allocation within the file,
    /// replacing the simple EOF-bump allocator.
    pub(crate) free_space: FreeSpaceTracker,

    /// Space releases deferred until their BAT changes are durable.
    /// Uses generation-based stamping to coordinate with flush().
    pub(crate) deferred_releases: DeferredReleases,

    // Mode
    pub(crate) read_only: bool,

    /// Region table bytes to rewrite (set when the two on-disk copies
    /// don't match). Consumed by [`VhdxBuilder::writable`].
    region_rewrite_data: Option<F::Buffer>,

    /// Error state: once set, all I/O operations fail.
    /// Shared with log and apply tasks so they
    /// can poison the file directly on fatal error.
    pub(crate) failed: Arc<FailureFlag>,

    // Log task state (set when opened writable via VhdxBuilder::writable).
    pub(crate) log_state: Option<LogTaskState>,
}

/// Log pipeline state for a writable VHDX file.
///
/// Created during [`VhdxBuilder::writable`] and consumed by
/// [`VhdxFile::close`] / [`VhdxFile::abort`]. All fields are set
/// together when the log task is spawned.
pub(crate) struct LogTaskState {
    /// Handle to the spawned log task.
    log_task: pal_async::task::Task<()>,
    /// Handle to the spawned apply task.
    apply_task: pal_async::task::Task<()>,
    /// Flush sequencer for FSN-gated ordering.
    pub flush_sequencer: Arc<FlushSequencer>,
    /// Failable semaphore for log backpressure.
    pub log_permits: Arc<crate::log_permits::LogPermits>,
    /// LSN watermark published by the log task. `flush()` waits on this.
    pub logged_lsn: Arc<crate::lsn_watermark::LsnWatermark>,
}

impl<F: 'static + AsyncFile> VhdxFile<F> {
    /// Begin opening a VHDX file, returning a [`VhdxBuilder`] to configure
    /// options before finalizing with [`read_only()`](VhdxBuilder::read_only)
    /// or [`writable()`](VhdxBuilder::writable).
    pub fn open(file: F) -> VhdxBuilder<F> {
        VhdxBuilder {
            file,
            options: OpenOptions::new(),
        }
    }

    /// Internal open logic shared by [`VhdxBuilder::read_only`] and
    /// [`VhdxBuilder::writable`].
    ///
    /// Validates the file identifier, headers, region tables, and metadata.
    /// If the log GUID is non-zero (indicating a dirty log), replays the
    /// log to recover the file. Read-only opens with a dirty log return
    /// [`CorruptionType::LogReplayRequired`].
    async fn open_inner(
        file: F,
        read_only: bool,
        log_sender: Option<mesh::Sender<LogRequest<F::Buffer>>>,
        options: &OpenOptions,
    ) -> Result<Self, OpenError> {
        // 1. Validate minimum file size.
        let file_length = file.file_size().await.map_err(OpenErrorInner::Io)?;
        if file_length < format::HEADER_AREA_SIZE {
            return Err(CorruptionType::EmptyFile.into());
        }

        // 2. Validate the file identifier signature.
        validate_file_identifier(&file).await?;

        // 3. Parse dual headers.
        let mut header = parse_headers(&file, file_length).await?;

        // 4. If log_guid is non-zero, replay the log.
        if header.log_guid != Guid::ZERO {
            // A dirty log requires writing to the file to replay. If the caller
            // opened read-only, we cannot proceed — the metadata may be
            // inconsistent and we're not allowed to fix it.
            if read_only {
                return Err((CorruptionType::LogReplayRequired).into());
            }

            // The file handle hasn't been Arc-wrapped yet — pass &file directly.
            let log_region = LogRegion {
                file_offset: header.log_offset,
                length: header.log_length,
            };

            let replay_result = log::replay_log(&file, &log_region, header.log_guid).await?;

            if replay_result.replayed {
                // Write a clean header: clear log_guid, bump sequence number.
                let new_seq = header.sequence_number + 1;
                let (buf, write_offset) = serialize_header(
                    &file,
                    new_seq,
                    header.file_write_guid,
                    header.data_write_guid,
                    Guid::ZERO,
                    header.log_offset,
                    header.log_length,
                    header.first_header_current,
                );
                file.write_from(write_offset, buf)
                    .await
                    .map_err(OpenErrorInner::Io)?;
                file.flush().await.map_err(OpenErrorInner::Io)?;

                // Update the in-flight header state for the rest of the open path.
                header.sequence_number = new_seq;
                header.log_guid = Guid::ZERO;
                header.first_header_current = !header.first_header_current;
            }
        }

        // 5. Parse region tables.
        let regions = parse_region_tables(&file).await?;

        // 6. Read metadata table.
        let metadata_table =
            MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length).await?;

        // 7. Verify known metadata (all required system items are recognized).
        verify_known_metadata(&metadata_table, false)?;

        // 8. Wrap file in Arc for shared access.
        let file = Arc::new(file);

        // 9. Create PageCache and register tags before reading metadata items.
        let mut cache = PageCache::new(
            file.clone(),
            log_sender.map(crate::log_task::LogClient::new),
            None,
            0,
        );
        cache.register_tag(BAT_TAG, regions.bat_offset);
        cache.register_tag(METADATA_TAG, regions.metadata_offset);
        cache.register_tag(SBM_TAG, 0);

        // 10. Read known metadata values.
        let known = read_known_metadata(&cache, &metadata_table).await?;

        // 11. Create BAT manager.
        let mut bat = Bat::new(
            known.disk_size,
            known.block_size,
            known.logical_sector_size,
            known.has_parent,
            regions.bat_length,
        )?;

        // 12. Create FreeSpaceTracker.
        let (free_space, mut eof_state) = FreeSpaceTracker::new(
            file_length,
            known.block_size,
            options.block_alignment,
            format::HEADER_AREA_SIZE,
            header.log_offset,
            header.log_length,
            regions.bat_offset,
            regions.bat_length,
            regions.metadata_offset,
            regions.metadata_length,
            bat.data_block_count,
        )?;

        // 13. Load in-memory BAT from disk.
        bat.load_bat_state(
            &*file,
            regions.bat_offset,
            regions.bat_length,
            &free_space,
            &mut eof_state,
        )
        .await?;

        // 14. Finalize free space initialization after BAT parse.
        free_space.complete_initialization(&eof_state);

        // 15. Construct VhdxFile.
        Ok(VhdxFile {
            file,
            cache,
            bat,
            disk_size: known.disk_size,
            block_size: known.block_size,
            logical_sector_size: known.logical_sector_size,
            physical_sector_size: known.physical_sector_size,
            has_parent: known.has_parent,
            is_fully_allocated: known.leave_blocks_allocated,
            page_83_data: known.page_83_data,
            metadata_table,
            header_state: HeaderState::new(&header),
            allocation_lock: futures::lock::Mutex::new(eof_state),
            allocation_event: event_listener::Event::new(),
            free_space,
            deferred_releases: DeferredReleases::new(),

            read_only,
            region_rewrite_data: regions.rewrite_data,
            failed: Arc::new(FailureFlag::new()),

            log_state: None,
        })
    }

    /// Open an existing VHDX file in read-only mode.
    ///
    /// If [`OpenOptions::allow_replay`] is true, a dirty log is replayed
    /// (requires the file handle to support writes for the replay I/O),
    /// but the resulting `VhdxFile` is still read-only. If false, a dirty
    /// log returns [`CorruptionType::LogReplayRequired`].
    async fn open_read_only(file: F, options: &OpenOptions) -> Result<Self, OpenError> {
        if options.allow_replay {
            let mut vhdx = Self::open_inner(file, false, None, options).await?;
            vhdx.read_only = true;
            Ok(vhdx)
        } else {
            Self::open_inner(file, true, None, options).await
        }
    }

    /// Open an existing VHDX file in writable mode with a log task.
    ///
    /// Replays a dirty log if needed, then spawns a log task for
    /// crash-consistent metadata writes. The log task receives dirty pages
    /// on `flush()` and writes them as WAL entries.
    ///
    /// The spawner must implement [`pal_async::task::Spawn`] to spawn the
    /// background log task.
    ///
    /// Call [`close()`](Self::close) for a clean shutdown. Dropping without
    /// close leaves the VHDX file dirty (log will be replayed on next open).
    async fn open_writable(
        file: F,
        spawner: &impl pal_async::task::Spawn,
        options: &OpenOptions,
    ) -> Result<Self, OpenError> {
        // Create mesh channel before open_inner so the cache gets the
        // sender at construction time.
        let (tx, rx) = mesh::channel::<LogRequest<F::Buffer>>();
        let mut vhdx = Self::open_inner(file, false, Some(tx.clone()), options).await?;

        // Create shared state for log task communication.
        let flush_sequencer = {
            let mut fs = FlushSequencer::new();
            fs.set_failure_flag(vhdx.failed.clone());
            Arc::new(fs)
        };
        let log_permits = Arc::new(crate::log_permits::LogPermits::new(
            // Permit count is a multiple of MAX_COMMIT_PAGES to allow
            // pipelining: multiple batches can be in-flight (committed
            // but not yet applied) simultaneously. Permits are released
            // by the apply task, not at commit time.
            crate::cache::MAX_COMMIT_PAGES * 4,
        ));
        let logged_lsn = Arc::new(crate::lsn_watermark::LsnWatermark::new());

        // Initialize the log writer.
        let log_guid = Guid::new_random();
        let (log_offset, log_length) = vhdx.header_state.log_region();
        let log_region = LogRegion {
            file_offset: log_offset,
            length: log_length,
        };
        let file_length = vhdx.file.file_size().await.map_err(OpenErrorInner::Io)?;
        let log_writer =
            log::LogWriter::initialize(vhdx.file.as_ref(), log_region, log_guid, file_length)
                .await?;

        // Write header with log_guid set (marks file as dirty).
        // This is done BEFORE spawning the log task so the file is marked
        // dirty before any log entries are written.
        vhdx.header_state
            .set_log_guid(log_guid, vhdx.file.as_ref(), None)
            .await
            .map_err(OpenErrorInner::Io)?;

        // Spawn the apply task.
        let applied_lsn = Arc::new(crate::lsn_watermark::LsnWatermark::new());
        let (apply_tx, apply_rx) = mesh::channel::<crate::apply_task::ApplyBatch<F::Buffer>>();
        let apply_task = spawner.spawn(
            "vhdx-apply-task",
            crate::apply_task::run_apply_task(
                apply_rx,
                vhdx.file.clone(),
                flush_sequencer.clone(),
                applied_lsn.clone(),
                log_permits.clone(),
                vhdx.failed.clone(),
            ),
        );

        // Spawn the log task.
        let task = spawner.spawn(
            "vhdx-log-task",
            crate::log_task::LogTask::new(
                vhdx.file.clone(),
                log_writer,
                flush_sequencer.clone(),
                log_permits.clone(),
                logged_lsn.clone(),
                applied_lsn.clone(),
                apply_tx,
                vhdx.failed.clone(),
            )
            .run(rx),
        );

        // Set log state on the cache.
        vhdx.cache.set_log_state(crate::cache::CacheLogState {
            permits: log_permits.clone(),
            applied_lsn: applied_lsn.clone(),
        });

        vhdx.log_state = Some(LogTaskState {
            log_task: task,
            apply_task,
            flush_sequencer,
            log_permits,
            logged_lsn,
        });

        // Repair mismatched region tables through the write-ahead log.
        // The pages enter the log pipeline and will be applied in due
        // course; the next caller-initiated flush() covers them via LSN
        // ordering. If we crash before that, either log replay applies
        // the entry or the mismatch is re-detected on reopen.
        if let Some(table_data) = vhdx.region_rewrite_data.take() {
            crate::region::rewrite_region_tables(
                &vhdx.cache,
                &vhdx
                    .log_state
                    .as_ref()
                    .expect("writable file has log_state")
                    .log_permits,
                table_data,
            )
            .await
            .map_err(OpenErrorInner::PipelineFailed)?;
        }

        Ok(vhdx)
    }

    /// Gracefully close the VHDX file.
    ///
    /// Flushes all dirty pages through the log, applies all logged entries,
    /// clears the log GUID in the header, and waits for the log task to exit.
    ///
    /// After this returns, the file is in a clean state (no log replay needed
    /// on next open).
    ///
    /// If no log task is running (read-only or opened without log), this is
    /// a no-op.
    pub async fn close(mut self) -> Result<(), VhdxIoError> {
        if let Some(state) = self.log_state.take() {
            // Ship any remaining dirty pages to the log task.
            // This is fire-and-forget — the Close RPC below will
            // process after this batch due to channel ordering.
            self.cache.commit().map_err(VhdxIoErrorInner::CommitCache)?;

            // Take the log client out of the cache to get the sender.
            let client = self
                .cache
                .take_log_client()
                .expect("log client disappeared");

            // Send Close RPC — the log task will log+apply all pending
            // batches, then respond.
            client.close().await?;

            state.log_task.await;
            // The log task dropping its apply_tx closes the apply channel,
            // causing the apply task to exit.
            state.apply_task.await;

            // Clear log GUID in the header now that the log is fully drained.
            // Done BEFORE truncation so that a crash during truncation
            // doesn't leave a non-zero log GUID pointing at a file that
            // may have been partially shrunk. With the GUID cleared first,
            // a crash at any later point just leaves a larger-than-necessary
            // file — no replay is attempted.
            self.header_state
                .clear_log_guid(self.file.as_ref(), Some(state.flush_sequencer.as_ref()))
                .await
                .map_err(VhdxIoErrorInner::WriteHeader)?;

            // Truncate the file to reclaim unused trailing space.
            // Best-effort: if this fails, the file is still correct,
            // just not compacted.
            if let Err(e) = self.truncate_file().await {
                tracing::warn!(
                    error = &e as &dyn std::error::Error,
                    "failed to truncate VHDX file on close"
                );
            }
        }
        Ok(())
    }

    /// Abort the VHDX file without graceful close.
    ///
    /// Drops the log channel (causing the log task to exit on its next
    /// recv) and waits for the log task to finish. No pending batches are
    /// applied and the log GUID is NOT cleared — the file remains dirty,
    /// requiring log replay on the next open.
    ///
    /// This is the test-friendly equivalent of a crash: all state held by
    /// the log task (including its `Arc<F>`) is released, but no new I/O
    /// is issued.
    pub async fn abort(mut self) {
        // Drop the log client so the log task's recv() returns Err.
        self.cache.take_log_client();

        // Wait for the log task to notice the closed channel and exit.
        // The log task dropping its apply_tx closes the apply channel too.
        if let Some(state) = self.log_state.take() {
            state.log_task.await;
            state.apply_task.await;
        }
    }
}

impl<F: AsyncFile> VhdxFile<F> {
    /// Virtual disk size in bytes.
    pub fn disk_size(&self) -> u64 {
        self.disk_size
    }

    /// Block size in bytes.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Logical sector size (512 or 4096).
    pub fn logical_sector_size(&self) -> u32 {
        self.logical_sector_size
    }

    /// Physical sector size (512 or 4096).
    pub fn physical_sector_size(&self) -> u32 {
        self.physical_sector_size
    }

    /// Whether this is a differencing disk (has a parent).
    pub fn has_parent(&self) -> bool {
        self.has_parent
    }

    /// Read and parse the parent locator from the metadata region.
    ///
    /// Returns `Ok(None)` for base (non-differencing) disks.
    /// Returns an error if the locator item is missing or corrupt.
    pub async fn parent_locator(&self) -> Result<Option<crate::locator::ParentLocator>, OpenError> {
        if !self.has_parent {
            return Ok(None);
        }
        let locator_data = self
            .metadata_table
            .read_item(&self.cache, false, &format::PARENT_LOCATOR_ITEM_GUID)
            .await?;
        Ok(Some(crate::locator::ParentLocator::parse(&locator_data)?))
    }

    /// Whether the disk was created with all blocks pre-allocated (fixed VHD).
    pub fn is_fully_allocated(&self) -> bool {
        self.is_fully_allocated
    }

    /// SCSI VPD Page 83 identifier (stable disk identity).
    pub fn page_83_data(&self) -> Guid {
        self.page_83_data
    }

    /// GUID changed on every virtual-disk data write.
    pub fn data_write_guid(&self) -> Guid {
        self.header_state.data_write_guid()
    }

    /// Whether the file was opened in read-only mode.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Ensures the requested write mode is enabled, updating the header
    /// and flushing if needed. If the current mode already satisfies the
    /// request, this is a no-op.
    ///
    /// Hot path (mode already enabled): single atomic load, no lock.
    /// Cold path (mode transition): acquires the header async mutex,
    /// generates new GUIDs, writes the header, flushes, then publishes.
    pub(crate) async fn enable_write_mode(&self, mode: WriteMode) -> Result<(), std::io::Error> {
        let flush_sequencer = self.log_state.as_ref().map(|s| s.flush_sequencer.as_ref());
        self.header_state
            .enable_write_mode(mode, self.file.as_ref(), flush_sequencer)
            .await
    }
}

/// Validate the file identifier signature at offset 0.
async fn validate_file_identifier(file: &impl AsyncFile) -> Result<(), OpenError> {
    // Read a full sector (not just the identifier struct) so that O_DIRECT
    // backends with sector-alignment requirements work correctly.
    let buf = file.alloc_buffer(4096);
    let buf = file.read_into(0, buf).await.map_err(OpenErrorInner::Io)?;

    let ident = FileIdentifier::read_from_prefix(buf.as_ref())
        .map_err(|_| CorruptionType::InvalidFileIdentifier)?
        .0;

    if ident.signature != format::FILE_IDENTIFIER_SIGNATURE {
        return Err(CorruptionType::InvalidFileIdentifier.into());
    }

    Ok(())
}

/// Shared failure flag for poisoning the VHDX file from any task.
///
/// Uses an `AtomicBool` for the fast path (`check`) and a mutex for
/// the error message. Once set, the flag is never cleared.
pub(crate) struct FailureFlag {
    flag: std::sync::atomic::AtomicBool,
    message: Mutex<Option<PipelineFailed>>,
}

impl FailureFlag {
    pub fn new() -> Self {
        Self {
            flag: std::sync::atomic::AtomicBool::new(false),
            message: Mutex::new(None),
        }
    }

    /// Check whether the flag is set. Fast path: single atomic load.
    pub fn check(&self) -> Result<(), VhdxIoError> {
        if self.flag.load(std::sync::atomic::Ordering::Relaxed)
            && let Some(msg) = self.message.lock().clone()
        {
            return Err(VhdxIoErrorInner::Failed(msg).into());
        }
        Ok(())
    }

    /// Set the failure flag. First caller's message wins.
    pub fn set(&self, error: &dyn std::error::Error) {
        let mut msg = self.message.lock();
        if msg.is_none() {
            *msg = Some(PipelineFailed(error.to_string()));
        }
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::create::{self, CreateParams};
    use crate::error::OpenError;
    use crate::format::BatEntry;
    use crate::format::BatEntryState;
    use crate::format::Header;
    use crate::format::MB1;
    use crate::space::AllocateFlags;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;
    use zerocopy::IntoBytes;

    #[async_test]
    async fn open_default_vhdx() {
        let (file, params) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        assert_eq!(vhdx.disk_size(), format::GB1);
        assert_eq!(vhdx.block_size(), format::DEFAULT_BLOCK_SIZE);
        assert_eq!(vhdx.logical_sector_size(), 512);
        assert_eq!(vhdx.physical_sector_size(), 512);
        assert!(!vhdx.has_parent());
        assert!(!vhdx.is_fully_allocated());
        assert!(vhdx.is_read_only());
        assert_ne!(vhdx.data_write_guid(), Guid::ZERO);
        assert_eq!(vhdx.data_write_guid(), params.data_write_guid);
    }

    #[async_test]
    async fn open_4k_sector_vhdx() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            logical_sector_size: 4096,
            physical_sector_size: 4096,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert_eq!(vhdx.logical_sector_size(), 4096);
        assert_eq!(vhdx.physical_sector_size(), 4096);
    }

    #[async_test]
    async fn open_512_sector_vhdx() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            logical_sector_size: 512,
            physical_sector_size: 512,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert_eq!(vhdx.logical_sector_size(), 512);
        assert_eq!(vhdx.physical_sector_size(), 512);
    }

    #[async_test]
    async fn open_various_block_sizes() {
        for &block_size in &[
            MB1 as u32,
            2 * MB1 as u32,
            32 * MB1 as u32,
            256 * MB1 as u32,
        ] {
            let file = InMemoryFile::new(0);
            let mut params = CreateParams {
                disk_size: format::GB1,
                block_size,
                ..Default::default()
            };
            create::create(&file, &mut params).await.unwrap();

            let vhdx = VhdxFile::open(file).read_only().await.unwrap();
            assert_eq!(vhdx.block_size(), block_size);
        }
    }

    #[async_test]
    async fn open_differencing_disk() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(vhdx.has_parent());
    }

    #[async_test]
    async fn open_fully_allocated() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            is_fully_allocated: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(vhdx.is_fully_allocated());
    }

    #[async_test]
    async fn open_dirty_log_no_valid_entries() {
        // Setting log_guid to a random GUID without writing matching log
        // entries causes replay_log to return NoValidLogEntries.
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Overwrite header 2's log_guid with a non-zero GUID, then fix the CRC.
        let mut buf = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_2, &mut buf)
            .await
            .unwrap();

        let mut header = Header::read_from_prefix(&buf).unwrap().0.clone();
        header.log_guid = Guid::new_random();
        header.checksum = 0;

        let header_bytes = header.as_bytes();
        buf[..header_bytes.len()].copy_from_slice(header_bytes);
        let crc = format::compute_checksum(&buf, 4);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());
        file.write_at(format::HEADER_OFFSET_2, &buf).await.unwrap();

        let result = VhdxFile::open(file).allow_replay(true).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::NoValidLogEntries
            )))
        ));
    }

    #[async_test]
    async fn open_invalid_file_identifier() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Corrupt the file identifier signature.
        file.write_at(0, b"BADMAGIC").await.unwrap();

        let result = VhdxFile::open(file).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::InvalidFileIdentifier
            )))
        ));
    }

    #[async_test]
    async fn open_empty_file() {
        // File smaller than HEADER_AREA_SIZE (1 MiB).
        let file = InMemoryFile::new(512);
        let result = VhdxFile::open(file).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::EmptyFile
            )))
        ));
    }

    #[async_test]
    async fn open_bat_block_lookup() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        // A newly created dynamic disk has all blocks as NotPresent.
        let mapping = vhdx.bat.get_block_mapping(0);
        assert_eq!(mapping.bat_state(), BatEntryState::NotPresent);
        assert_eq!(mapping.file_offset(), 0);
    }

    #[async_test]
    async fn open_bat_all_blocks_default() {
        let disk_size = 4 * MB1; // Small disk → 2 blocks.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        let block_count = (disk_size / vhdx.block_size() as u64) as u32;

        for block in 0..block_count {
            let mapping = vhdx.bat.get_block_mapping(block);
            assert_eq!(mapping.bat_state(), BatEntryState::NotPresent);
            assert_eq!(mapping.file_offset(), 0);
        }
    }

    #[async_test]
    async fn open_read_only_flag() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(vhdx.is_read_only());
    }

    #[async_test]
    async fn open_populates_in_memory_bat() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        // All payload entries should be NotPresent.
        for i in 0..vhdx.bat.data_block_count {
            assert_eq!(
                vhdx.bat.get_block_mapping(i).bat_state(),
                BatEntryState::NotPresent,
                "block {i} should be NotPresent"
            );
        }
    }

    #[async_test]
    async fn open_with_allocated_blocks() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = parse_region_tables(&file).await.unwrap();

        // Manually write a FullyPresent BAT entry for block 0 at offset 4 MB
        // (just after the metadata region, within the file).
        // First extend the file to cover the block (4 MB offset + 2 MB block = 6 MB).
        file.set_file_size(6 * MB1).await.unwrap();

        let entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(4);
        file.write_at(regions.bat_offset, entry.as_bytes())
            .await
            .unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        let mapping = vhdx.bat.get_block_mapping(0);
        assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent,);
        assert_eq!(mapping.file_megabyte(), 4);
    }

    #[async_test]
    async fn bat_lookup_is_synchronous() {
        // Compile-time verification: get_block_mapping() is a regular fn,
        // not an async fn. We call it without .await.
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        let mapping = vhdx.bat.get_block_mapping(0);
        assert_eq!(mapping.bat_state(), BatEntryState::NotPresent);
    }

    #[async_test]
    async fn eof_counter_no_overlap() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open_inner(file, false, None, &OpenOptions::new())
            .await
            .unwrap();
        let mut eof = vhdx.allocation_lock.lock().await;
        let a = vhdx
            .allocate_space(&mut eof, MB1 as u32, AllocateFlags::new())
            .await
            .unwrap();
        let b = vhdx
            .allocate_space(&mut eof, MB1 as u32, AllocateFlags::new())
            .await
            .unwrap();
        // Two allocations must not overlap.
        assert_ne!(a.file_offset, b.file_offset);
        assert!(b.file_offset >= a.file_offset + MB1);
    }

    #[async_test]
    async fn eof_counter_mb_aligned() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open_inner(file, false, None, &OpenOptions::new())
            .await
            .unwrap();
        let mut eof = vhdx.allocation_lock.lock().await;
        let result = vhdx
            .allocate_space(&mut eof, MB1 as u32, AllocateFlags::new())
            .await
            .unwrap();
        assert_eq!(result.file_offset % MB1, 0, "offset must be MB1-aligned");
    }

    #[async_test]
    async fn open_with_allocated_blocks_inits_space() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = parse_region_tables(&file).await.unwrap();

        // Extend file to 8 MB then write a FullyPresent BAT entry at offset 4 MB.
        file.set_file_size(8 * MB1).await.unwrap();

        let entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(4);
        file.write_at(regions.bat_offset, entry.as_bytes())
            .await
            .unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        // The free space tracker should have offset 4*MB marked as in-use.
        let eof = vhdx.allocation_lock.lock().await;
        assert!(
            vhdx.free_space
                .is_range_in_use(&eof, 4 * MB1, vhdx.block_size())
        );
    }

    #[async_test]
    async fn non_differencing_no_locator() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(!vhdx.has_parent());
        assert!(vhdx.parent_locator().await.unwrap().is_none());
    }

    /// Helper: inject a parent locator metadata entry and blob into a diff disk.
    ///
    /// Reads the existing metadata table, appends a new entry for the parent
    /// locator GUID, writes the locator blob at the entry's data offset, and
    /// updates the metadata table header's entry count.
    async fn inject_parent_locator(file: &InMemoryFile, locator_blob: &[u8]) {
        use crate::format::{MetadataTableEntry, MetadataTableEntryFlags, MetadataTableHeader};
        use zerocopy::{FromBytes, IntoBytes};

        let regions = parse_region_tables(file).await.unwrap();

        // Read the full metadata table (first 64 KiB of metadata region).
        let mut table_buf = vec![0u8; format::METADATA_TABLE_SIZE as usize];
        file.read_at(regions.metadata_offset, &mut table_buf)
            .await
            .unwrap();

        // Parse header to get current entry count.
        let mut header = MetadataTableHeader::read_from_prefix(&table_buf)
            .unwrap()
            .0
            .clone();
        let old_count = header.entry_count as usize;
        let entry_size = size_of::<MetadataTableEntry>();
        let header_size = size_of::<MetadataTableHeader>();

        // Find the max data offset used by existing entries to place our blob after them.
        let mut max_data_end: u32 = format::METADATA_TABLE_SIZE as u32;
        for i in 0..old_count {
            let off = header_size + i * entry_size;
            let entry = MetadataTableEntry::read_from_prefix(&table_buf[off..])
                .unwrap()
                .0
                .clone();
            if entry.length > 0 {
                let end = entry.offset + entry.length;
                if end > max_data_end {
                    max_data_end = end;
                }
            }
        }

        // Place the parent locator blob right after existing data.
        let locator_offset = max_data_end;

        // Write the new entry.
        let new_entry = MetadataTableEntry {
            item_id: format::PARENT_LOCATOR_ITEM_GUID,
            offset: locator_offset,
            length: locator_blob.len() as u32,
            flags: MetadataTableEntryFlags::new().with_is_required(true),
            reserved2: 0,
        };
        let new_entry_file_offset = header_size + old_count * entry_size;
        let e_bytes = new_entry.as_bytes();
        table_buf[new_entry_file_offset..new_entry_file_offset + e_bytes.len()]
            .copy_from_slice(e_bytes);

        // Update header entry count.
        header.entry_count = (old_count + 1) as u16;
        let h_bytes = header.as_bytes();
        table_buf[..h_bytes.len()].copy_from_slice(h_bytes);

        // Write back the metadata table.
        file.write_at(regions.metadata_offset, &table_buf)
            .await
            .unwrap();

        // Write the locator blob into the metadata region data area.
        file.write_at(
            regions.metadata_offset + locator_offset as u64,
            locator_blob,
        )
        .await
        .unwrap();
    }

    #[async_test]
    async fn differencing_has_locator() {
        use crate::locator;

        // Create a differencing disk.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        // Build a parent locator blob and inject it into the metadata region.
        let locator_blob = locator::build_locator(
            format::PARENT_LOCATOR_VHDX_TYPE_GUID,
            &[
                ("parent_linkage", "{some-guid}"),
                ("relative_path", ".\\parent.vhdx"),
                ("absolute_win32_path", "C:\\VMs\\parent.vhdx"),
            ],
        );
        inject_parent_locator(&file, &locator_blob).await;

        // Open and verify.
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(vhdx.has_parent());

        let loc = vhdx
            .parent_locator()
            .await
            .unwrap()
            .expect("should have locator");
        assert_eq!(loc.locator_type, format::PARENT_LOCATOR_VHDX_TYPE_GUID);
        assert_eq!(loc.find("parent_linkage"), Some("{some-guid}"));
        assert_eq!(loc.find("relative_path"), Some(".\\parent.vhdx"));
        assert_eq!(
            loc.find("absolute_win32_path"),
            Some("C:\\VMs\\parent.vhdx")
        );
    }

    #[async_test]
    async fn parent_paths_extraction() {
        use crate::locator;

        // Create a differencing disk with a parent locator.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let locator_blob = locator::build_locator(
            format::PARENT_LOCATOR_VHDX_TYPE_GUID,
            &[
                ("parent_linkage", "{some-guid}"),
                ("relative_path", ".\\parent.vhdx"),
                ("absolute_win32_path", "C:\\VMs\\parent.vhdx"),
            ],
        );
        inject_parent_locator(&file, &locator_blob).await;

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        let loc = vhdx
            .parent_locator()
            .await
            .unwrap()
            .expect("should have locator");
        let paths = loc.parent_paths();
        assert_eq!(paths.parent_linkage.as_deref(), Some("{some-guid}"));
        assert_eq!(paths.relative_path.as_deref(), Some(".\\parent.vhdx"));
        assert_eq!(
            paths.absolute_win32_path.as_deref(),
            Some("C:\\VMs\\parent.vhdx")
        );
        assert!(paths.volume_path.is_none());
    }

    #[async_test]
    async fn differencing_missing_locator_errors() {
        // Create a diff disk but don't write any locator data.
        // create() doesn't add a parent locator entry, so read_item() will
        // return MissingRequiredMetadata.
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(vhdx.has_parent());
        let result = vhdx.parent_locator().await;
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Log replay integration tests
    // -----------------------------------------------------------------------

    /// Inject a dirty log into a VHDX file:
    /// 1. Write log entries using LogWriter
    /// 2. Set the header's log_guid to match
    /// 3. Update header CRC
    ///
    /// Returns the log_guid used.
    async fn inject_dirty_log(
        file: &InMemoryFile,
        data_pages: &[log::DataPage<'_>],
        zero_ranges: &[log::ZeroRange],
    ) -> Guid {
        // Read the active header (header 2, sequence_number=1 after create).
        let mut hdr_buf = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_2, &mut hdr_buf)
            .await
            .unwrap();
        let header = Header::read_from_prefix(&hdr_buf).unwrap().0.clone();

        let log_guid = Guid::new_random();
        let log_region = LogRegion {
            file_offset: header.log_offset,
            length: header.log_length,
        };

        // Initialize a LogWriter and write the entry.
        let file_size = file.file_size().await.unwrap();
        let mut writer = log::LogWriter::initialize(file, log_region, log_guid, file_size)
            .await
            .unwrap();

        if !data_pages.is_empty() || !zero_ranges.is_empty() {
            writer
                .write_entry(file, data_pages, zero_ranges)
                .await
                .unwrap();
        }

        // Set log_guid in a new header with bumped sequence number.
        // Write to header 1 (the non-current slot) with a higher sequence
        // number so it becomes the active header.
        let mut header_copy = header;
        header_copy.log_guid = log_guid;
        header_copy.sequence_number += 1;
        header_copy.checksum = 0;

        let mut buf = vec![0u8; format::HEADER_SIZE as usize];
        let hdr_bytes = header_copy.as_bytes();
        buf[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
        let crc = format::compute_checksum(&buf, 4);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());

        // Write to header 1 (which now has a higher seq, becoming active).
        file.write_at(format::HEADER_OFFSET_1, &buf).await.unwrap();

        log_guid
    }

    #[async_test]
    async fn open_replays_dirty_log_data() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Pick a target offset >= LOGABLE_OFFSET (192 KiB = region table offset).
        // Use 320 KiB (= 5 * 64 KiB) to be past both region tables.
        let target_offset: u64 = 5 * format::KB64;

        // Build a recognizable data pattern.
        let pattern = [0xABu8; 4096];
        let data_page = log::DataPage {
            file_offset: target_offset,
            payload: &pattern,
        };

        inject_dirty_log(&file, &[data_page], &[]).await;

        // Open should replay the log and succeed.
        let vhdx = VhdxFile::open(file)
            .allow_replay(true)
            .read_only()
            .await
            .unwrap();
        assert_eq!(vhdx.disk_size(), format::GB1);

        // Verify the data pattern was written at the target offset via the
        // Arc<InMemoryFile> inside the VhdxFile.
        let mut readback = [0u8; 4096];
        vhdx.file
            .read_at(target_offset, &mut readback)
            .await
            .unwrap();
        assert_eq!(readback, pattern);
    }

    #[async_test]
    async fn open_replays_dirty_log_zeros() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        // Write non-zero data at a target offset first.
        let target_offset: u64 = 5 * format::KB64;
        let non_zero = [0xFFu8; 4096];
        file.write_at(target_offset, &non_zero).await.unwrap();

        // Inject a dirty log with a zero descriptor targeting that offset.
        let zero_range = log::ZeroRange {
            file_offset: target_offset,
            length: 4096,
        };

        inject_dirty_log(&file, &[], &[zero_range]).await;

        // Open should replay the log and succeed.
        let vhdx = VhdxFile::open(file)
            .allow_replay(true)
            .read_only()
            .await
            .unwrap();
        assert_eq!(vhdx.disk_size(), format::GB1);

        // Verify the range is now zeroed.
        let mut readback = [0u8; 4096];
        vhdx.file
            .read_at(target_offset, &mut readback)
            .await
            .unwrap();
        assert_eq!(readback, [0u8; 4096]);
    }

    #[async_test]
    async fn open_replay_then_reopen_clean() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        let target_offset: u64 = 5 * format::KB64;
        let pattern = [0xCDu8; 4096];
        let data_page = log::DataPage {
            file_offset: target_offset,
            payload: &pattern,
        };

        inject_dirty_log(&file, &[data_page], &[]).await;

        // First open triggers replay.
        let vhdx = VhdxFile::open(file)
            .allow_replay(true)
            .read_only()
            .await
            .unwrap();
        // The clean header was written to the file inside vhdx.
        // Make a snapshot of the replayed file for the second open.
        let snapshot = vhdx.file.snapshot();
        drop(vhdx);

        // Create a new InMemoryFile from the snapshot for the second open.
        let file3 = InMemoryFile::from_snapshot(snapshot);

        // Second open should succeed without replay (log_guid is now ZERO).
        let vhdx2 = VhdxFile::open(file3).read_only().await.unwrap();
        assert_eq!(vhdx2.disk_size(), format::GB1);
    }

    #[async_test]
    async fn open_replay_corrupt_log_entry() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        let target_offset: u64 = 5 * format::KB64;
        let pattern = [0xEEu8; 4096];
        let data_page = log::DataPage {
            file_offset: target_offset,
            payload: &pattern,
        };

        let _log_guid = inject_dirty_log(&file, &[data_page], &[]).await;

        // Read the active header to find the log region offset.
        let mut hdr_buf = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_1, &mut hdr_buf)
            .await
            .unwrap();
        let header = Header::read_from_prefix(&hdr_buf).unwrap().0.clone();

        // Corrupt the first byte of the log region (flip a byte in the CRC
        // of the log entry).
        let mut corrupt_buf = [0u8; 1];
        file.read_at(header.log_offset + 4, &mut corrupt_buf)
            .await
            .unwrap();
        corrupt_buf[0] ^= 0xFF;
        file.write_at(header.log_offset + 4, &corrupt_buf)
            .await
            .unwrap();

        // Open should fail because there are no valid log entries for this GUID.
        let result = VhdxFile::open(file).allow_replay(true).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::NoValidLogEntries
            )))
        ));
    }

    #[async_test]
    async fn open_read_only_dirty_log_rejected() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;

        let target_offset: u64 = 5 * format::KB64;
        let pattern = [0xBBu8; 4096];
        let data_page = log::DataPage {
            file_offset: target_offset,
            payload: &pattern,
        };

        inject_dirty_log(&file, &[data_page], &[]).await;

        // Read-only open with a dirty log should return LogReplayRequired.
        let result = VhdxFile::open(file).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::LogReplayRequired
            )))
        ));
    }
}
