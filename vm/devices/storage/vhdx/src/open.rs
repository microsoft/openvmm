// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX file open orchestration.
//!
//! Ties together file identifier validation, header parsing, log replay,
//! region table parsing, metadata parsing, BAT/free-space setup, cache tag
//! setup, and writable log task startup. Guest-visible I/O is added in later
//! chunks of the split review series.

#![allow(dead_code)]

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
pub struct VhdxBuilder<F> {
    file: F,
    options: OpenOptions,
}

#[derive(Debug, Clone)]
struct OpenOptions {
    /// Block data alignment in bytes. BAT/free-space allocation consumes this
    /// in a later chunk; this chunk validates and preserves the option shape.
    block_alignment: u32,
    /// Whether to allow log replay on a read-only open.
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
    /// Must be 0 or a power of 2. The allocator-specific behavior arrives
    /// with the BAT/free-space substrate.
    pub fn block_alignment(mut self, alignment: u32) -> Self {
        self.options.block_alignment = alignment;
        self
    }

    /// Allow log replay when opening read-only.
    ///
    /// When true, a dirty log is replayed (the file handle must support
    /// writes for the replay I/O) but the resulting [`VhdxFile`] is still
    /// read-only. When false, a dirty log returns an error.
    pub fn allow_replay(mut self, allow: bool) -> Self {
        self.options.allow_replay = allow;
        self
    }

    /// Open the VHDX file in read-only mode.
    pub async fn read_only(self) -> Result<VhdxFile<F>, OpenError> {
        VhdxFile::open_read_only(self.file, &self.options).await
    }

    /// Open the VHDX file in writable mode with a log task.
    ///
    /// Replays a dirty log if needed, marks the file dirty by setting the
    /// active log GUID, and spawns the log/apply pipeline used by the cache.
    pub async fn writable(
        self,
        spawner: &impl pal_async::task::Spawn,
    ) -> Result<VhdxFile<F>, OpenError> {
        VhdxFile::open_writable(self.file, spawner, &self.options).await
    }
}

/// An open VHDX file handle.
///
/// Created via [`VhdxFile::open()`], which returns a [`VhdxBuilder`] for
/// configuring options before calling
/// [`read_only()`](VhdxBuilder::read_only) or
/// [`writable()`](VhdxBuilder::writable).
pub struct VhdxFile<F: AsyncFile> {
    pub(crate) file: Arc<F>,
    pub(crate) cache: PageCache<F>,
    pub(crate) bat: Bat,

    disk_size: u64,
    block_size: u32,
    logical_sector_size: u32,
    physical_sector_size: u32,
    has_parent: bool,
    is_fully_allocated: bool,
    page_83_data: Guid,

    metadata_table: MetadataTable,
    pub(crate) header_state: HeaderState,
    pub(crate) allocation_lock: futures::lock::Mutex<EofState>,
    pub(crate) allocation_event: event_listener::Event,
    pub(crate) free_space: FreeSpaceTracker,
    pub(crate) deferred_releases: DeferredReleases,
    pub(crate) read_only: bool,
    region_rewrite_data: Option<F::Buffer>,
    pub(crate) failed: Arc<FailureFlag>,
    pub(crate) log_state: Option<LogTaskState>,
}

/// Log pipeline state for a writable VHDX file.
pub(crate) struct LogTaskState {
    log_task: pal_async::task::Task<()>,
    apply_task: pal_async::task::Task<()>,
    pub flush_sequencer: Arc<FlushSequencer>,
    pub log_permits: Arc<crate::log_permits::LogPermits>,
    pub logged_lsn: Arc<crate::lsn_watermark::LsnWatermark>,
}

impl<F: 'static + AsyncFile> VhdxFile<F> {
    /// Begin opening a VHDX file, returning a builder for open options.
    pub fn open(file: F) -> VhdxBuilder<F> {
        VhdxBuilder {
            file,
            options: OpenOptions::new(),
        }
    }

    async fn open_inner(
        file: F,
        read_only: bool,
        log_sender: Option<mesh::Sender<LogRequest<F::Buffer>>>,
        options: &OpenOptions,
    ) -> Result<Self, OpenError> {
        if options.block_alignment != 0 && !options.block_alignment.is_power_of_two() {
            return Err(OpenErrorInner::InvalidParameter(
                crate::error::InvalidFormatReason::BlockAlignmentNotPowerOfTwo,
            )
            .into());
        }

        let file_length = file.file_size().await.map_err(OpenErrorInner::Io)?;
        if file_length < format::HEADER_AREA_SIZE {
            return Err(CorruptionType::EmptyFile.into());
        }

        validate_file_identifier(&file).await?;

        let mut header = parse_headers(&file, file_length).await?;

        if header.log_guid != Guid::ZERO {
            if read_only {
                return Err(CorruptionType::LogReplayRequired.into());
            }

            let log_region = LogRegion {
                file_offset: header.log_offset,
                length: header.log_length,
            };
            let replay_result = log::replay_log(&file, &log_region, header.log_guid).await?;

            if replay_result.replayed {
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

                header.sequence_number = new_seq;
                header.log_guid = Guid::ZERO;
                header.first_header_current = !header.first_header_current;
            }
        }

        let regions = parse_region_tables(&file).await?;
        let metadata_table =
            MetadataTable::read(&file, regions.metadata_offset, regions.metadata_length).await?;
        verify_known_metadata(&metadata_table, false)?;

        let file = Arc::new(file);
        let mut cache = PageCache::new(
            file.clone(),
            log_sender.map(crate::log_task::LogClient::new),
            None,
            0,
        );
        cache.register_tag(BAT_TAG, regions.bat_offset);
        cache.register_tag(METADATA_TAG, regions.metadata_offset);

        let known = read_known_metadata(&cache, &metadata_table).await?;

        let mut bat = Bat::new(
            known.disk_size,
            known.block_size,
            known.logical_sector_size,
            known.has_parent,
            regions.bat_length,
        )?;

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

        bat.load_bat_state(
            file.as_ref(),
            regions.bat_offset,
            regions.bat_length,
            &free_space,
            &mut eof_state,
        )
        .await?;
        free_space.complete_initialization(&eof_state);

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

    async fn open_read_only(file: F, options: &OpenOptions) -> Result<Self, OpenError> {
        if options.allow_replay {
            let mut vhdx = Self::open_inner(file, false, None, options).await?;
            vhdx.read_only = true;
            Ok(vhdx)
        } else {
            Self::open_inner(file, true, None, options).await
        }
    }

    async fn open_writable(
        file: F,
        spawner: &impl pal_async::task::Spawn,
        options: &OpenOptions,
    ) -> Result<Self, OpenError> {
        let (tx, rx) = mesh::channel::<LogRequest<F::Buffer>>();
        let mut vhdx = Self::open_inner(file, false, Some(tx), options).await?;

        let flush_sequencer = {
            let mut fs = FlushSequencer::new();
            fs.set_failure_flag(vhdx.failed.clone());
            Arc::new(fs)
        };
        let log_permits = Arc::new(crate::log_permits::LogPermits::new(
            crate::cache::MAX_COMMIT_PAGES * 4,
        ));
        let logged_lsn = Arc::new(crate::lsn_watermark::LsnWatermark::new());

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

        vhdx.header_state
            .set_log_guid(log_guid, vhdx.file.as_ref(), None)
            .await
            .map_err(OpenErrorInner::Io)?;

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

        let log_task = spawner.spawn(
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

        vhdx.cache.set_log_state(crate::cache::CacheLogState {
            permits: log_permits.clone(),
            applied_lsn: applied_lsn.clone(),
        });

        vhdx.log_state = Some(LogTaskState {
            log_task,
            apply_task,
            flush_sequencer,
            log_permits,
            logged_lsn,
        });

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
    /// Flushes dirty metadata through the log pipeline, waits for the log and
    /// apply tasks to drain, and clears the log GUID in the header. File
    /// truncation is deferred until the free-space substrate lands.
    pub async fn close(mut self) -> Result<(), VhdxIoError> {
        if let Some(state) = self.log_state.take() {
            self.cache.commit().map_err(VhdxIoErrorInner::CommitCache)?;

            let client = self
                .cache
                .take_log_client()
                .expect("log client disappeared");
            client.close().await?;

            state.log_task.await;
            state.apply_task.await;

            self.header_state
                .clear_log_guid(self.file.as_ref(), Some(state.flush_sequencer.as_ref()))
                .await
                .map_err(VhdxIoErrorInner::WriteHeader)?;
        }
        Ok(())
    }

    /// Abort the VHDX file without graceful close.
    ///
    /// Drops the log channel and waits for background tasks to exit. The log
    /// GUID is not cleared, so a subsequent open must replay the log.
    pub async fn abort(mut self) {
        self.cache.take_log_client();
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

    /// Whether the disk was created with all blocks pre-allocated.
    pub fn is_fully_allocated(&self) -> bool {
        self.is_fully_allocated
    }

    /// SCSI VPD Page 83 identifier.
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

    pub(crate) async fn enable_write_mode(&self, mode: WriteMode) -> Result<(), std::io::Error> {
        let flush_sequencer = self.log_state.as_ref().map(|s| s.flush_sequencer.as_ref());
        self.header_state
            .enable_write_mode(mode, self.file.as_ref(), flush_sequencer)
            .await
    }
}

async fn validate_file_identifier(file: &impl AsyncFile) -> Result<(), OpenError> {
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

    /// Check whether the flag is set.
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
    use crate::log::DataPage;
    use crate::log::ZeroRange;
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
    async fn open_invalid_file_identifier() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
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
    async fn invalid_block_alignment_is_rejected() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let result = VhdxFile::open(file).block_alignment(3).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::InvalidParameter(
                crate::error::InvalidFormatReason::BlockAlignmentNotPowerOfTwo
            )))
        ));
    }

    #[async_test]
    async fn open_read_only_flag() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        assert!(vhdx.is_read_only());
    }

    #[async_test]
    async fn open_bat_block_lookup() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        let mapping = vhdx.bat.get_block_mapping(0);
        assert_eq!(mapping.bat_state(), BatEntryState::NotPresent);
        assert_eq!(mapping.file_offset(), 0);
    }

    #[async_test]
    async fn open_populates_in_memory_bat() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open(file).read_only().await.unwrap();

        for block in 0..vhdx.bat.data_block_count {
            assert_eq!(
                vhdx.bat.get_block_mapping(block).bat_state(),
                BatEntryState::NotPresent,
                "block {block} should be NotPresent"
            );
        }
    }

    #[async_test]
    async fn open_with_allocated_blocks() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = parse_region_tables(&file).await.unwrap();

        file.set_file_size(6 * MB1).await.unwrap();
        let entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(4);
        file.write_at(regions.bat_offset, entry.as_bytes())
            .await
            .unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        let mapping = vhdx.bat.get_block_mapping(0);
        assert_eq!(mapping.bat_state(), BatEntryState::FullyPresent);
        assert_eq!(mapping.file_megabyte(), 4);
    }

    #[async_test]
    async fn eof_counter_no_overlap() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let vhdx = VhdxFile::open_inner(file, false, None, &OpenOptions::new())
            .await
            .unwrap();
        let mut eof = vhdx.allocation_lock.lock().await;
        let first = vhdx
            .allocate_space(&mut eof, MB1 as u32, AllocateFlags::new())
            .await
            .unwrap();
        let second = vhdx
            .allocate_space(&mut eof, MB1 as u32, AllocateFlags::new())
            .await
            .unwrap();

        assert_ne!(first.file_offset, second.file_offset);
        assert!(second.file_offset >= first.file_offset + MB1);
    }

    #[async_test]
    async fn open_with_allocated_blocks_inits_space() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let regions = parse_region_tables(&file).await.unwrap();

        file.set_file_size(8 * MB1).await.unwrap();
        let entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(4);
        file.write_at(regions.bat_offset, entry.as_bytes())
            .await
            .unwrap();

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
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

    async fn inject_parent_locator(file: &InMemoryFile, locator_blob: &[u8]) {
        use crate::format::MetadataTableEntry;
        use crate::format::MetadataTableEntryFlags;
        use crate::format::MetadataTableHeader;

        let regions = parse_region_tables(file).await.unwrap();
        let mut table_buf = vec![0u8; format::METADATA_TABLE_SIZE as usize];
        file.read_at(regions.metadata_offset, &mut table_buf)
            .await
            .unwrap();

        let mut header = MetadataTableHeader::read_from_prefix(&table_buf)
            .unwrap()
            .0
            .clone();
        let old_count = header.entry_count as usize;
        let entry_size = size_of::<MetadataTableEntry>();
        let header_size = size_of::<MetadataTableHeader>();

        let mut max_data_end: u32 = format::METADATA_TABLE_SIZE as u32;
        for i in 0..old_count {
            let off = header_size + i * entry_size;
            let entry = MetadataTableEntry::read_from_prefix(&table_buf[off..])
                .unwrap()
                .0
                .clone();
            if entry.length > 0 {
                max_data_end = max_data_end.max(entry.offset + entry.length);
            }
        }

        let new_entry = MetadataTableEntry {
            item_id: format::PARENT_LOCATOR_ITEM_GUID,
            offset: max_data_end,
            length: locator_blob.len() as u32,
            flags: MetadataTableEntryFlags::new().with_is_required(true),
            reserved2: 0,
        };
        let new_entry_file_offset = header_size + old_count * entry_size;
        let e_bytes = new_entry.as_bytes();
        table_buf[new_entry_file_offset..new_entry_file_offset + e_bytes.len()]
            .copy_from_slice(e_bytes);

        header.entry_count = (old_count + 1) as u16;
        let h_bytes = header.as_bytes();
        table_buf[..h_bytes.len()].copy_from_slice(h_bytes);

        file.write_at(regions.metadata_offset, &table_buf)
            .await
            .unwrap();
        file.write_at(regions.metadata_offset + max_data_end as u64, locator_blob)
            .await
            .unwrap();
    }

    #[async_test]
    async fn differencing_has_locator() {
        let file = InMemoryFile::new(0);
        let mut params = CreateParams {
            disk_size: format::GB1,
            has_parent: true,
            ..Default::default()
        };
        create::create(&file, &mut params).await.unwrap();

        let locator_blob = crate::locator::build_locator(
            format::PARENT_LOCATOR_VHDX_TYPE_GUID,
            &[
                ("parent_linkage", "{some-guid}"),
                ("relative_path", ".\\parent.vhdx"),
            ],
        );
        inject_parent_locator(&file, &locator_blob).await;

        let vhdx = VhdxFile::open(file).read_only().await.unwrap();
        let loc = vhdx
            .parent_locator()
            .await
            .unwrap()
            .expect("should have locator");
        assert_eq!(loc.locator_type, format::PARENT_LOCATOR_VHDX_TYPE_GUID);
        assert_eq!(loc.find("parent_linkage"), Some("{some-guid}"));
        assert_eq!(loc.find("relative_path"), Some(".\\parent.vhdx"));
    }

    async fn inject_dirty_log(
        file: &InMemoryFile,
        data_pages: &[DataPage<'_>],
        zero_ranges: &[ZeroRange],
    ) -> Guid {
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

        let mut header_copy = header;
        header_copy.log_guid = log_guid;
        header_copy.sequence_number += 1;
        header_copy.checksum = 0;

        let mut buf = vec![0u8; format::HEADER_SIZE as usize];
        let hdr_bytes = header_copy.as_bytes();
        buf[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
        let crc = format::compute_checksum(&buf, 4);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());
        file.write_at(format::HEADER_OFFSET_1, &buf).await.unwrap();

        log_guid
    }

    #[async_test]
    async fn open_read_only_dirty_log_rejected() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let pattern = [0xBBu8; 4096];
        let data_page = DataPage {
            file_offset: 5 * format::KB64,
            payload: &pattern,
        };
        inject_dirty_log(&file, &[data_page], &[]).await;

        let result = VhdxFile::open(file).read_only().await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::LogReplayRequired
            )))
        ));
    }

    #[async_test]
    async fn open_replays_dirty_log_data() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let target_offset = 5 * format::KB64;
        let pattern = [0xABu8; 4096];
        let data_page = DataPage {
            file_offset: target_offset,
            payload: &pattern,
        };
        inject_dirty_log(&file, &[data_page], &[]).await;

        let vhdx = VhdxFile::open(file)
            .allow_replay(true)
            .read_only()
            .await
            .unwrap();
        let mut readback = [0u8; 4096];
        vhdx.file
            .read_at(target_offset, &mut readback)
            .await
            .unwrap();
        assert_eq!(readback, pattern);
    }

    #[async_test]
    async fn open_replays_dirty_log_zeros() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let target_offset = 5 * format::KB64;
        file.write_at(target_offset, &[0xFFu8; 4096]).await.unwrap();
        let zero_range = ZeroRange {
            file_offset: target_offset,
            length: 4096,
        };
        inject_dirty_log(&file, &[], &[zero_range]).await;

        let vhdx = VhdxFile::open(file)
            .allow_replay(true)
            .read_only()
            .await
            .unwrap();
        let mut readback = [0u8; 4096];
        vhdx.file
            .read_at(target_offset, &mut readback)
            .await
            .unwrap();
        assert_eq!(readback, [0u8; 4096]);
    }

    #[async_test]
    async fn writable_open_and_close_reopens_clean(driver: pal_async::DefaultDriver) {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_ref = {
            let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
            assert!(!vhdx.is_read_only());
            assert!(vhdx.log_state.is_some());
            let file_ref = vhdx.file.clone();
            vhdx.close().await.unwrap();
            file_ref
        };

        let reopened = VhdxFile::open(InMemoryFile::from_snapshot(file_ref.snapshot()))
            .read_only()
            .await
            .unwrap();
        assert!(reopened.is_read_only());
    }

    #[async_test]
    async fn abort_leaves_dirty_log(driver: pal_async::DefaultDriver) {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_ref = {
            let vhdx = VhdxFile::open(file).writable(&driver).await.unwrap();
            let file_ref = vhdx.file.clone();
            vhdx.abort().await;
            file_ref
        };

        let result = VhdxFile::open(InMemoryFile::from_snapshot(file_ref.snapshot()))
            .read_only()
            .await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::LogReplayRequired
            )))
        ));
    }
}
