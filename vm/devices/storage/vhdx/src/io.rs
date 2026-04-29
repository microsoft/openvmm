// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX read/write I/O resolution and guards.
//!
//! Translates guest virtual disk offsets into file-level ranges via
//! [`VhdxFile::resolve_read`] and [`VhdxFile::resolve_write`], handling
//! block allocation, TFP lifecycle, sector bitmap updates, and
//! crash-consistent BAT commits.

use crate::AsyncFile;
use crate::bat::BatGuard;
use crate::bat::BlockMapping;
use crate::bat::BlockSpan;
use crate::bat::BlockType;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::format::BatEntryState;
use crate::format::MB1;
use crate::header::WriteMode;
use crate::open::VhdxFile;
use crate::space::AllocateFlags;

/// Record of a block that had Transitioning-to-Fully-Present (TFP) set
/// during the allocation phase of [`VhdxFile::resolve_write`].
///
/// Carried inside [`WriteIoGuard`] so that `complete()` can finalize
/// the BAT without re-walking the block range, and `abort()` can revert
/// without guessing which blocks were modified.
struct TfpRecord {
    /// Block number in the BAT.
    block_number: u32,
    /// The block's mapping before TFP was set. Used by the abort path
    /// to revert the in-memory BAT.
    original_mapping: BlockMapping,
    /// File offset of newly allocated space, if any. `None` when TFP
    /// was set on an already-allocated block (e.g. PartiallyPresent ->
    /// FullyPresent promotion). The abort path releases this space back
    /// to the free pool.
    allocated_offset: Option<u64>,
}

/// Resolved range from a read operation.
///
/// Each range describes a contiguous portion of the read request and its
/// data source. The caller iterates these ranges to perform the actual I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRange {
    /// Data present at this file offset. Caller should read from the VHDX file.
    Data {
        /// Byte offset within the virtual disk.
        guest_offset: u64,
        /// Length in bytes.
        length: u32,
        /// Byte offset within the VHDX file where the data lives.
        file_offset: u64,
    },
    /// Range is zero-filled. Caller should return zeros.
    Zero {
        /// Byte offset within the virtual disk.
        guest_offset: u64,
        /// Length in bytes.
        length: u32,
    },
    /// Range is unmapped (transparent to parent). Caller should read from
    /// the parent disk in a differencing chain.
    Unmapped {
        /// Byte offset within the virtual disk.
        guest_offset: u64,
        /// Length in bytes.
        length: u32,
    },
}

/// Resolved range from a write operation.
///
/// Each range describes a contiguous portion of the write target and
/// what the caller needs to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteRange {
    /// Write caller's data at this file offset.
    Data {
        /// Byte offset within the virtual disk.
        guest_offset: u64,
        /// Length in bytes.
        length: u32,
        /// Byte offset within the VHDX file where data should be written.
        file_offset: u64,
    },
    /// Zero-fill this file range (e.g. newly allocated block padding).
    Zero {
        /// Byte offset within the VHDX file to zero-fill.
        file_offset: u64,
        /// Length in bytes.
        length: u32,
    },
}

impl<F: AsyncFile> VhdxFile<F> {
    /// Validate an I/O request and acquire per-block refcounts.
    ///
    /// Checks the failure flag, alignment, and bounds. Then increments
    /// per-block refcounts atomically, waiting if trim has claimed any
    /// block. Returns the [`BatGuard`] that holds the refcounts.
    ///
    /// Callers must handle zero-length requests before calling this.
    async fn validate_and_acquire(
        &self,
        offset: u64,
        len: u32,
    ) -> Result<BatGuard<'_>, VhdxIoError> {
        self.failed.check()?;

        if !offset.is_multiple_of(self.logical_sector_size() as u64)
            || !(len as u64).is_multiple_of(self.logical_sector_size() as u64)
        {
            return Err(VhdxIoErrorInner::UnalignedIo.into());
        }

        if offset
            .checked_add(len as u64)
            .is_none_or(|end| end > self.disk_size())
        {
            return Err(VhdxIoErrorInner::BeyondEndOfDisk.into());
        }

        let start_block = self.bat.offset_to_block(offset);
        let end_block = self.bat.offset_to_block(offset + len as u64 - 1);
        let block_count = end_block - start_block + 1;

        let guard = self
            .bat
            .acquire_io_refcounts(start_block, block_count)
            .await;

        Ok(guard)
    }

    /// Resolve a read request into file-level ranges.
    pub async fn resolve_read(
        &self,
        offset: u64,
        len: u32,
        ranges: &mut Vec<ReadRange>,
    ) -> Result<ReadIoGuard<'_, F>, VhdxIoError> {
        if len == 0 {
            return Ok(ReadIoGuard::empty());
        }

        let guard = self.validate_and_acquire(offset, len).await?;

        for span in self.bat.block_spans(offset, len) {
            let mapping = self.bat.get_block_mapping(span.block_number);

            match mapping.bat_state() {
                BatEntryState::FullyPresent => {
                    let file_offset = mapping.file_offset() + span.block_offset as u64;
                    ranges.push(ReadRange::Data {
                        guest_offset: span.virtual_offset,
                        length: span.length,
                        file_offset,
                    });
                }
                BatEntryState::PartiallyPresent => {
                    self.resolve_partial_block_read(
                        mapping.file_offset(),
                        span.virtual_offset,
                        span.length,
                        ranges,
                    )
                    .await?;
                }
                BatEntryState::NotPresent => {
                    if self.has_parent() {
                        ranges.push(ReadRange::Unmapped {
                            guest_offset: span.virtual_offset,
                            length: span.length,
                        });
                    } else {
                        ranges.push(ReadRange::Zero {
                            guest_offset: span.virtual_offset,
                            length: span.length,
                        });
                    }
                }
                BatEntryState::Zero | BatEntryState::Unmapped | BatEntryState::Undefined => {
                    ranges.push(ReadRange::Zero {
                        guest_offset: span.virtual_offset,
                        length: span.length,
                    });
                }
            }
        }

        Ok(ReadIoGuard::new(guard))
    }

    /// Resolve a write request into file-level ranges.
    pub async fn resolve_write(
        &self,
        offset: u64,
        len: u32,
        ranges: &mut Vec<WriteRange>,
    ) -> Result<WriteIoGuard<'_, F>, VhdxIoError> {
        if self.is_read_only() {
            return Err(VhdxIoErrorInner::ReadOnly.into());
        }

        if len == 0 {
            return Ok(WriteIoGuard::new_completed(self));
        }

        self.enable_write_mode(WriteMode::DataWritable)
            .await
            .map_err(VhdxIoErrorInner::WriteHeader)?;

        let refcount_guard = self.validate_and_acquire(offset, len).await?;

        let mut blocks_needing_allocation: Vec<BlockSpan> = Vec::new();

        for span in self.bat.block_spans(offset, len) {
            let is_full_block = span.is_full_block(self.block_size());

            loop {
                let (state, file_offset, has_tfp) = {
                    let mapping = self.bat.get_block_mapping(span.block_number);
                    (
                        mapping.bat_state(),
                        mapping.file_offset(),
                        mapping.transitioning_to_fully_present(),
                    )
                };

                if has_tfp {
                    let listener = self.allocation_event.listen();
                    if self
                        .bat
                        .get_block_mapping(span.block_number)
                        .transitioning_to_fully_present()
                    {
                        listener.await;
                    }
                    continue;
                }

                match state {
                    BatEntryState::FullyPresent => {
                        ranges.push(WriteRange::Data {
                            guest_offset: span.virtual_offset,
                            length: span.length,
                            file_offset: file_offset + span.block_offset as u64,
                        });
                        break;
                    }
                    BatEntryState::PartiallyPresent if !is_full_block => {
                        ranges.push(WriteRange::Data {
                            guest_offset: span.virtual_offset,
                            length: span.length,
                            file_offset: file_offset + span.block_offset as u64,
                        });
                        break;
                    }
                    BatEntryState::PartiallyPresent => {
                        blocks_needing_allocation.push(span);
                        break;
                    }
                    BatEntryState::NotPresent
                    | BatEntryState::Zero
                    | BatEntryState::Unmapped
                    | BatEntryState::Undefined => {
                        blocks_needing_allocation.push(span);
                        break;
                    }
                }
            }
        }

        if blocks_needing_allocation.is_empty() {
            return Ok(WriteIoGuard::new_no_alloc(
                self,
                refcount_guard,
                offset,
                len,
            ));
        }

        let mut alloc_guard = loop {
            let alloc_guard = self.allocation_lock.lock().await;

            let listener = self.allocation_event.listen();
            if !blocks_needing_allocation.iter().any(|span| {
                self.bat
                    .get_block_mapping(span.block_number)
                    .transitioning_to_fully_present()
            }) {
                break alloc_guard;
            }
            drop(alloc_guard);
            listener.await;
        };

        let mut tfp_records: Vec<TfpRecord> = Vec::new();
        let mut needs_flush_before_log = false;

        let eof = &mut *alloc_guard;
        let allocation_result: Result<(), VhdxIoError> = async {
            for span in &blocks_needing_allocation {
                let is_full_block = span.is_full_block(self.block_size());
                let mapping = self.bat.get_block_mapping(span.block_number);

                assert!(
                    !mapping.transitioning_to_fully_present(),
                    "block {} has TFP after overlap wait",
                    span.block_number
                );

                match mapping.bat_state() {
                    BatEntryState::FullyPresent => {
                        ranges.push(WriteRange::Data {
                            guest_offset: span.virtual_offset,
                            length: span.length,
                            file_offset: mapping.file_offset() + span.block_offset as u64,
                        });
                    }
                    BatEntryState::PartiallyPresent if is_full_block => {
                        let original = mapping;
                        let new_mapping = original.with_transitioning_to_fully_present(true);

                        self.bat.set_block_mapping(span.block_number, new_mapping);

                        tfp_records.push(TfpRecord {
                            block_number: span.block_number,
                            original_mapping: original,
                            allocated_offset: None,
                        });

                        ranges.push(WriteRange::Data {
                            guest_offset: span.virtual_offset,
                            length: span.length,
                            file_offset: mapping.file_offset() + span.block_offset as u64,
                        });
                    }
                    _ => {
                        let original = mapping;
                        let (new_offset, space_state) = self
                            .allocate_block_space(span.block_number, mapping, eof)
                            .await?;

                        if is_full_block {
                            self.allocate_full_block(
                                span,
                                original,
                                new_offset,
                                space_state,
                                &mut tfp_records,
                                &mut needs_flush_before_log,
                                ranges,
                            );
                        } else {
                            self.allocate_partial_block(
                                span,
                                mapping,
                                new_offset,
                                space_state,
                                eof,
                                ranges,
                            )
                            .await?;
                        }
                    }
                }
            }

            Ok(())
        }
        .await;

        if let Err(e) = allocation_result {
            self.abort_write_sync(&tfp_records);
            return Err(e);
        }

        Ok(WriteIoGuard::new(
            self,
            refcount_guard,
            offset,
            len,
            needs_flush_before_log,
            tfp_records,
        ))
    }

    async fn allocate_block_space(
        &self,
        block_number: u32,
        mapping: BlockMapping,
        eof: &mut crate::space::EofState,
    ) -> Result<(u64, crate::space::SpaceState), VhdxIoError> {
        if let Some(deferred_offset) = self.deferred_releases.remove(block_number) {
            return Ok((deferred_offset, crate::space::SpaceState::OwnStale));
        }

        if mapping.is_soft_anchored() {
            let old_file_offset = mapping.file_offset();
            if self.free_space.unmark_trimmed_block(
                block_number,
                old_file_offset,
                self.block_size(),
            ) {
                return Ok((old_file_offset, crate::space::SpaceState::OwnStale));
            }
        }

        let r = self
            .allocate_space(eof, self.block_size(), AllocateFlags::new())
            .await?;
        Ok((r.file_offset, r.state))
    }

    fn allocate_full_block(
        &self,
        span: &BlockSpan,
        original_mapping: BlockMapping,
        new_offset: u64,
        space_state: crate::space::SpaceState,
        tfp_records: &mut Vec<TfpRecord>,
        needs_flush_before_log: &mut bool,
        ranges: &mut Vec<WriteRange>,
    ) {
        let new_mapping = BlockMapping::new()
            .with_bat_state(original_mapping.bat_state())
            .with_transitioning_to_fully_present(true)
            .with_file_megabyte((new_offset / MB1) as u32);

        self.bat.set_block_mapping(span.block_number, new_mapping);

        tfp_records.push(TfpRecord {
            block_number: span.block_number,
            original_mapping,
            allocated_offset: Some(new_offset),
        });

        if !space_state.is_safe() {
            *needs_flush_before_log = true;
        }

        ranges.push(WriteRange::Data {
            guest_offset: span.virtual_offset,
            length: span.length,
            file_offset: new_offset + span.block_offset as u64,
        });
    }

    async fn allocate_partial_block(
        &self,
        span: &BlockSpan,
        mapping: BlockMapping,
        new_offset: u64,
        space_state: crate::space::SpaceState,
        eof: &mut crate::space::EofState,
        ranges: &mut Vec<WriteRange>,
    ) -> Result<(), VhdxIoError> {
        let is_partial_present =
            self.has_parent() && mapping.bat_state() == BatEntryState::NotPresent;

        if is_partial_present {
            self.ensure_sbm_allocated(span.block_number, eof).await?;
        }

        let new_state = if is_partial_present {
            BatEntryState::PartiallyPresent
        } else {
            BatEntryState::FullyPresent
        };

        let new_mapping = BlockMapping::new()
            .with_bat_state(new_state)
            .with_transitioning_to_fully_present(false)
            .with_file_megabyte((new_offset / MB1) as u32);

        let pre_log_fsn = if !space_state.is_safe() {
            self.log_state
                .as_ref()
                .map(|state| state.flush_sequencer.current_fsn())
        } else {
            None
        };

        self.bat
            .write_block_mapping(
                &self.cache,
                BlockType::Payload,
                span.block_number,
                new_mapping,
                pre_log_fsn,
            )
            .await?;

        if !is_partial_present && span.block_offset > 0 && !space_state.is_zero() {
            ranges.push(WriteRange::Zero {
                file_offset: new_offset,
                length: span.block_offset,
            });
        }

        ranges.push(WriteRange::Data {
            guest_offset: span.virtual_offset,
            length: span.length,
            file_offset: new_offset + span.block_offset as u64,
        });

        let end_offset = span.block_offset + span.length;
        if !is_partial_present && end_offset < self.block_size() && !space_state.is_zero() {
            ranges.push(WriteRange::Zero {
                file_offset: new_offset + end_offset as u64,
                length: self.block_size() - end_offset,
            });
        }

        Ok(())
    }

    async fn ensure_sbm_allocated(
        &self,
        block_number: u32,
        eof: &mut crate::space::EofState,
    ) -> Result<(), VhdxIoError> {
        let chunk_number = block_number / self.bat.chunk_ratio;
        let sbm_mapping = self.bat.get_sector_bitmap_mapping(chunk_number);

        if sbm_mapping.bat_state() == BatEntryState::FullyPresent {
            return Ok(());
        }

        let sbm_alloc = self
            .allocate_space(
                eof,
                crate::bat::SECTOR_BITMAP_BLOCK_SIZE,
                AllocateFlags::new().with_zero(true),
            )
            .await?;

        let new_sbm = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte((sbm_alloc.file_offset / MB1) as u32);

        self.bat
            .write_block_mapping(
                &self.cache,
                BlockType::SectorBitmap,
                chunk_number,
                new_sbm,
                None,
            )
            .await?;

        Ok(())
    }

    async fn complete_write_inner(
        &self,
        offset: u64,
        len: u32,
        tfp_records: &[TfpRecord],
        needs_flush_before_log: bool,
    ) -> Result<(), VhdxIoError> {
        let had_tfp = !tfp_records.is_empty();

        let pre_log_fsn = if needs_flush_before_log {
            self.log_state
                .as_ref()
                .map(|state| state.flush_sequencer.current_fsn())
        } else {
            None
        };

        for (i, record) in tfp_records.iter().enumerate() {
            let mapping = self.bat.get_block_mapping(record.block_number);
            let final_mapping = BlockMapping::new()
                .with_bat_state(BatEntryState::FullyPresent)
                .with_transitioning_to_fully_present(false)
                .with_file_megabyte(mapping.file_megabyte());

            if let Err(e) = self
                .bat
                .write_block_mapping(
                    &self.cache,
                    BlockType::Payload,
                    record.block_number,
                    final_mapping,
                    pre_log_fsn,
                )
                .await
            {
                self.abort_write_sync(&tfp_records[i..]);
                return Err(e);
            }
        }

        if had_tfp {
            self.allocation_event.notify(usize::MAX);
        }

        if self.has_parent() && len > 0 {
            for span in self.bat.block_spans(offset, len) {
                let mapping = self.bat.get_block_mapping(span.block_number);
                if !mapping.transitioning_to_fully_present()
                    && mapping.bat_state() == BatEntryState::PartiallyPresent
                {
                    self.set_sector_bitmap_bits(span.virtual_offset, span.length, true)
                        .await?;
                }
            }
        }

        Ok(())
    }

    fn abort_write_sync(&self, tfp_records: &[TfpRecord]) {
        if tfp_records.is_empty() {
            return;
        }

        for record in tfp_records {
            self.bat
                .set_block_mapping(record.block_number, record.original_mapping);
            if let Some(offset) = record.allocated_offset {
                self.free_space.release(offset, self.block_size());
            }
        }

        self.allocation_event.notify(usize::MAX);
    }

    /// Flush all writes to stable storage.
    pub async fn flush(&self) -> Result<(), VhdxIoError> {
        self.failed.check()?;

        if self.is_read_only() {
            return Err(VhdxIoErrorInner::ReadOnly.into());
        }

        let flush_gen = self.deferred_releases.stamp_uncommitted();

        let lsn = self.cache.commit().map_err(VhdxIoErrorInner::CommitCache)?;

        let state = self
            .log_state
            .as_ref()
            .expect("writable file has log_state");

        state
            .logged_lsn
            .wait_for(lsn)
            .await
            .map_err(VhdxIoErrorInner::Failed)?;

        state
            .flush_sequencer
            .flush(self.file.as_ref())
            .await
            .map_err(VhdxIoErrorInner::Flush)?;

        for (block_number, file_offset, size, anchor) in
            self.deferred_releases.drain_committed(flush_gen)
        {
            if anchor {
                let _ = self
                    .free_space
                    .mark_trimmed_block(block_number, file_offset, size);
            } else {
                self.free_space.release(file_offset, size);
            }
        }

        Ok(())
    }
}

/// Guard for read I/O. Drop after file reads are complete.
pub struct ReadIoGuard<'a, F: AsyncFile> {
    _bat_guard: BatGuard<'a>,
    _phantom: std::marker::PhantomData<&'a VhdxFile<F>>,
}

impl<'a, F: AsyncFile> ReadIoGuard<'a, F> {
    fn new(bat_guard: BatGuard<'a>) -> Self {
        Self {
            _bat_guard: bat_guard,
            _phantom: std::marker::PhantomData,
        }
    }

    fn empty() -> Self {
        Self {
            _bat_guard: BatGuard::empty(),
            _phantom: std::marker::PhantomData,
        }
    }
}

/// Guard for write I/O. Call [`complete()`](Self::complete) to finalize,
/// or drop to abort.
pub struct WriteIoGuard<'a, F: AsyncFile> {
    vhdx: &'a VhdxFile<F>,
    _bat_guard: BatGuard<'a>,
    offset: u64,
    len: u32,
    completed: bool,
    needs_flush_before_log: bool,
    tfp_records: Vec<TfpRecord>,
}

impl<'a, F: AsyncFile> WriteIoGuard<'a, F> {
    fn new(
        vhdx: &'a VhdxFile<F>,
        bat_guard: BatGuard<'a>,
        offset: u64,
        len: u32,
        needs_flush_before_log: bool,
        tfp_records: Vec<TfpRecord>,
    ) -> Self {
        Self {
            vhdx,
            _bat_guard: bat_guard,
            offset,
            len,
            completed: false,
            needs_flush_before_log,
            tfp_records,
        }
    }

    fn new_completed(vhdx: &'a VhdxFile<F>) -> Self {
        Self {
            vhdx,
            _bat_guard: BatGuard::empty(),
            offset: 0,
            len: 0,
            completed: true,
            needs_flush_before_log: false,
            tfp_records: Vec::new(),
        }
    }

    fn new_no_alloc(vhdx: &'a VhdxFile<F>, bat_guard: BatGuard<'a>, offset: u64, len: u32) -> Self {
        Self {
            vhdx,
            _bat_guard: bat_guard,
            offset,
            len,
            completed: false,
            needs_flush_before_log: false,
            tfp_records: Vec::new(),
        }
    }

    /// Finalize the write after data has been written to resolved ranges.
    pub async fn complete(mut self) -> Result<(), VhdxIoError> {
        self.completed = true;
        self.vhdx
            .complete_write_inner(
                self.offset,
                self.len,
                &self.tfp_records,
                self.needs_flush_before_log,
            )
            .await
    }
}

impl<F: AsyncFile> Drop for WriteIoGuard<'_, F> {
    fn drop(&mut self) {
        if !self.completed {
            self.vhdx.abort_write_sync(&self.tfp_records);
        }
    }
}
