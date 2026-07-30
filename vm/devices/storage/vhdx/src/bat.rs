// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! BAT (Block Allocation Table) lookup and management.
//!
//! Provides on-demand BAT entry lookup through the [`PageCache`], computing
//! the correct BAT page offset for any given block number. Handles the
//! interleaving of payload block entries with sector bitmap entries.

use crate::AsyncFile;
use crate::cache::PageCache;
use crate::cache::PageKey;
use crate::cache::WriteMode;
use crate::create::ceil_div;
use crate::create::chunk_block_count;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::error::OpenErrorInner;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::flush::Fsn;
use crate::format::BatEntry;
use crate::format::BatEntryState;
use crate::format::CACHE_PAGE_SIZE;
use crate::format::ENTRIES_PER_BAT_PAGE;
use crate::format::MB1;
use bitfield_struct::bitfield;
use std::sync::atomic::AtomicU16;
use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;
use zerocopy::IntoBytes;

use crate::space::EofState;
use crate::space::FreeSpaceTracker;
use zerocopy::FromBytes;

/// Cache tag for BAT region pages.
pub(crate) const BAT_TAG: u8 = 0;

/// Size of a sector bitmap block in bytes (1 MiB).
pub(crate) const SECTOR_BITMAP_BLOCK_SIZE: u32 = 1024 * 1024;

/// Per-block I/O refcount packed into a `u16`.
///
/// Layout:
/// - Bit 15 (`TRIM_PENDING_BIT`): set by trim to block new I/O acquisitions.
/// - Bits 0-14: I/O reference count (0..32767).
///
/// Valid states:
/// - `0x0000` — idle, no I/O, no trim.
/// - `0x0001..MAX_IO_REFCOUNT` — active I/O refcount.
/// - `TRIM_PENDING` (`0x8000`) — trim pending, I/Os drained, ready to claim.
/// - `0x8001..0xFFFE` — trim pending + draining I/Os.
/// - `TRIM_CLAIMED` (`0xFFFF`) — trim owns the block exclusively.
///
/// The pending bit gives trim **writer priority**: once set, no new I/O
/// can increment the refcount, preventing livelock from a steady I/O stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IoBlockRef(u16);

impl IoBlockRef {
    /// High bit: trim is pending (blocks new I/O).
    const TRIM_PENDING_BIT: u16 = 0x8000;
    /// Maximum I/O refcount (bits 0-14 all set).
    const MAX_IO_REFCOUNT: u16 = 0x7FFF;
    const FREE: Self = Self(0);
    /// Trim pending, all I/Os drained — ready to finish claiming.
    const TRIM_PENDING: Self = Self(Self::TRIM_PENDING_BIT);
    /// Trim has exclusively claimed the block.
    const TRIM_CLAIMED: Self = Self(u16::MAX);

    /// The I/O refcount (bits 0-14), ignoring the trim-pending bit.
    fn io_count(self) -> u16 {
        self.0 & Self::MAX_IO_REFCOUNT
    }

    /// Whether the trim-pending bit is set.
    fn trim_pending(self) -> bool {
        self.0 & Self::TRIM_PENDING_BIT != 0
    }

    /// Whether new I/O acquisitions should be blocked.
    ///
    /// True when the trim-pending bit is set OR the I/O count is at
    /// the maximum (would overflow into the pending bit).
    fn blocks_new_io(self) -> bool {
        self.0 >= Self::MAX_IO_REFCOUNT
    }
}

pub(crate) struct Bat {
    /// Number of data blocks (payload blocks) in the disk.
    pub data_block_count: u32,
    /// Number of sector bitmap blocks (chunks). Zero if no parent.
    pub sector_bitmap_block_count: u32,
    /// Chunk ratio: number of data blocks per sector bitmap entry.
    pub chunk_ratio: u32,
    /// Block size in bytes.
    pub block_size: u32,
    /// Whether the disk has a parent (differencing).
    pub has_parent: bool,

    /// One `AtomicU32` per payload block (indexed by block number).
    /// Each stores a [`BlockMapping`] bitfield. Lock-free: individual
    /// entries are read/written atomically without a shared lock.
    payload_mappings: Vec<AtomicU32>,
    /// One `AtomicU32` per sector bitmap block (indexed by chunk number).
    sector_bitmap_mappings: Vec<AtomicU32>,

    /// Per-payload-block I/O refcounts (see [`IoBlockRef`] for layout).
    io_refcounts: Vec<AtomicU16>,

    /// Notified whenever a block's refcount changes in a way that could
    /// unblock a waiter: I/O count reaching zero (unblocks trim),
    /// trim releasing a claim (unblocks I/O), or I/O count dropping
    /// below the overflow threshold (unblocks I/O).
    refcount_event: event_listener::Event,
}

/// In-memory BAT entry. Compact 32-bit representation used in the in-memory
/// BAT array (not on disk).
///
/// Layout: state (3 bits) | transitioning_to_fully_present (1 bit) | file_megabyte (28 bits)
///
/// The 28-bit `file_megabyte` field supports files up to 2^28 MB = 256 TB.
#[bitfield(u32)]
#[derive(PartialEq, Eq)]
pub(crate) struct BlockMapping {
    /// Block state (same values as BatEntryState).
    #[bits(3)]
    state: u8,
    /// Set during allocation: space has been allocated but data I/O may still
    /// be in flight. Other writers to this block must wait.
    #[bits(1)]
    pub transitioning_to_fully_present: bool,
    /// File offset in megabytes.
    #[bits(28)]
    pub file_megabyte: u32,
}

impl BlockMapping {
    fn supported_bat_entry_bits() -> u64 {
        u64::from(
            BatEntry::new()
                .with_state(0b111)
                .with_file_offset_mb((1_u64 << 44) - 1),
        )
    }

    /// File byte offset (converts the megabyte field to bytes).
    pub fn file_offset(self) -> u64 {
        self.file_megabyte() as u64 * MB1
    }

    /// Parse the block state.
    ///
    /// Panics if the raw state is invalid — this is an internal invariant
    /// since states are validated at BAT load time and only set to known
    /// values at runtime.
    pub fn bat_state(self) -> BatEntryState {
        BatEntryState::from_raw(self.state()).expect("InternalBlockMapping has invalid state")
    }

    pub fn with_bat_state(self, state: BatEntryState) -> Self {
        self.with_state(state as u8)
    }

    /// Whether this mapping is soft-anchored: unmapped or undefined
    /// with a non-zero file offset retained for potential reuse.
    pub fn is_soft_anchored(self) -> bool {
        let state = self.bat_state();
        matches!(state, BatEntryState::Unmapped | BatEntryState::Undefined)
            && self.file_megabyte() != 0
    }

    /// Create a [`BlockMapping`] from an on-disk [`BatEntry`].
    ///
    /// Validates the entry state and file offset. For non-differencing
    /// disks (`has_parent == false`), normalizes `PartiallyPresent` to
    /// `FullyPresent` at load time.
    pub fn from_bat_entry(entry: BatEntry, has_parent: bool) -> Result<Self, OpenError> {
        if u64::from(entry) & !Self::supported_bat_entry_bits() != 0 {
            return Err(CorruptionType::ReservedBatEntryFieldNonzero.into());
        }
        let raw_state = entry.state();
        let mut bat_state =
            BatEntryState::from_raw(raw_state).ok_or(CorruptionType::InvalidBlockState)?;
        // Normalize PartiallyPresent → FullyPresent for non-diff disks.
        if !has_parent && bat_state == BatEntryState::PartiallyPresent {
            bat_state = BatEntryState::FullyPresent;
        }
        let file_mb = entry.file_offset_mb();
        if file_mb > 0x0FFF_FFFF {
            return Err((CorruptionType::InvalidBlockState).into());
        }
        Ok(BlockMapping::new()
            .with_bat_state(bat_state)
            .with_transitioning_to_fully_present(false)
            .with_file_megabyte(file_mb as u32))
    }

    /// Create a [`BlockMapping`] from an on-disk SBM [`BatEntry`].
    ///
    /// Validates the entry state and file offset. Normalizes
    /// `PartiallyPresent` to `FullyPresent` (compatibility).
    pub fn from_sbm_bat_entry(entry: BatEntry) -> Result<Self, OpenError> {
        if u64::from(entry) & !Self::supported_bat_entry_bits() != 0 {
            return Err(CorruptionType::ReservedBatEntryFieldNonzero.into());
        }
        let raw_state = entry.state();

        let mut bat_state =
            BatEntryState::from_raw(raw_state).ok_or(CorruptionType::InvalidBlockState)?;
        if bat_state == BatEntryState::PartiallyPresent {
            bat_state = BatEntryState::FullyPresent;
        }
        let file_mb = entry.file_offset_mb();
        if file_mb > 0x0FFF_FFFF {
            return Err((CorruptionType::InvalidBlockState).into());
        }
        Ok(BlockMapping::new()
            .with_bat_state(bat_state)
            .with_transitioning_to_fully_present(false)
            .with_file_megabyte(file_mb as u32))
    }
}

/// Block type discriminator for BAT entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockType {
    /// A data payload block.
    Payload,
    /// A sector bitmap block (differencing disks only).
    SectorBitmap,
}

impl Bat {
    /// Create a new BAT manager from parsed metadata.
    ///
    /// Computes chunk ratio, data block count, and sector bitmap block count.
    pub fn new(
        disk_size: u64,
        block_size: u32,
        logical_sector_size: u32,
        has_parent: bool,
        bat_length: u32,
    ) -> Result<Self, OpenError> {
        let chunk_ratio = chunk_block_count(block_size, logical_sector_size);
        if chunk_ratio == 0 {
            return Err((CorruptionType::InvalidBlockSize).into());
        }

        let data_block_count = ceil_div(disk_size, block_size as u64) as u32;
        let sector_bitmap_block_count = if has_parent {
            ceil_div(data_block_count as u64, chunk_ratio as u64) as u32
        } else {
            0
        };

        let entry_count = if has_parent {
            sector_bitmap_block_count as u64 * (chunk_ratio as u64 + 1)
        } else {
            data_block_count as u64
                + (data_block_count.saturating_sub(1) as u64 / chunk_ratio as u64)
        };

        let required_bytes = entry_count * size_of::<BatEntry>() as u64;
        if required_bytes > bat_length as u64 {
            return Err((CorruptionType::BatTooSmall).into());
        }

        let payload_mappings = (0..data_block_count).map(|_| AtomicU32::new(0)).collect();
        let sector_bitmap_mappings = (0..sector_bitmap_block_count)
            .map(|_| AtomicU32::new(0))
            .collect();
        let io_refcounts = (0..data_block_count).map(|_| AtomicU16::new(0)).collect();

        Ok(Bat {
            data_block_count,
            sector_bitmap_block_count,
            chunk_ratio,
            block_size,
            has_parent,
            payload_mappings,
            sector_bitmap_mappings,
            io_refcounts,
            refcount_event: event_listener::Event::new(),
        })
    }

    /// Try to atomically increment the I/O refcount for a block.
    ///
    /// Returns `true` if the increment succeeded, `false` if new I/O is
    /// blocked. New I/O is blocked when:
    /// - The trim-pending bit is set (trim has writer priority).
    /// - The I/O count is at `MAX_IO_REFCOUNT` (would overflow).
    /// - The block is trim-claimed (`TRIM_CLAIMED`).
    fn try_increment_io_refcount(&self, block_number: u32) -> bool {
        let rc = &self.io_refcounts[block_number as usize];
        loop {
            let old = IoBlockRef(rc.load(Ordering::Acquire));
            if old.blocks_new_io() {
                return false;
            }
            let new = old.0 + 1;
            match rc.compare_exchange_weak(old.0, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(_) => continue,
            }
        }
    }

    /// Atomically decrement the I/O refcount.
    ///
    /// The trim-pending bit is preserved — only the I/O count in
    /// bits 0-14 is decremented. Panics on underflow or if the block
    /// is trim-claimed.
    ///
    /// Returns `true` if callers should notify `refcount_event`:
    /// when the I/O count hits zero (trim may be waiting), or when
    /// the count drops from the overflow threshold.
    #[must_use]
    fn decrement_io_refcount(&self, block_number: u32) -> bool {
        let prev =
            IoBlockRef(self.io_refcounts[block_number as usize].fetch_sub(1, Ordering::AcqRel));
        assert!(
            prev.io_count() > 0 && prev != IoBlockRef::TRIM_CLAIMED,
            "io_refcount underflow or trim claimed on block {block_number} (was {:#06x})",
            prev.0,
        );
        prev.io_count() == 1 || prev.io_count() == IoBlockRef::MAX_IO_REFCOUNT
    }

    /// Claim a block for trim, with writer priority.
    pub(crate) async fn claim_for_trim(&self, block_number: u32) -> TrimGuard<'_> {
        let rc = &self.io_refcounts[block_number as usize];
        loop {
            let listener = self.refcount_event.listen();
            let result = rc.fetch_update(Ordering::AcqRel, Ordering::Acquire, |raw| {
                let old = IoBlockRef(raw);
                match old {
                    // Idle — claim directly.
                    IoBlockRef::FREE => Some(IoBlockRef::TRIM_CLAIMED.0),
                    // Pending bit set, I/Os drained — finish claiming.
                    IoBlockRef::TRIM_PENDING => Some(IoBlockRef::TRIM_CLAIMED.0),
                    // Already claimed — wait for release.
                    IoBlockRef::TRIM_CLAIMED => None,
                    // I/Os active, no pending bit — set it.
                    _ if !old.trim_pending() => Some(old.0 | IoBlockRef::TRIM_PENDING_BIT),
                    // Pending bit set, I/Os still draining — wait.
                    _ => None,
                }
            });
            break match result.map(IoBlockRef) {
                Ok(IoBlockRef::FREE | IoBlockRef::TRIM_PENDING) => TrimGuard {
                    bat: self,
                    block_number,
                },
                _ => {
                    // Wait for the I/O count to reach 0 or for the trim claim to be released.
                    listener.await;
                    continue;
                }
            };
        }
    }

    /// Release a trim claim on a block (store 0), waking blocked I/O paths.
    fn release_trim_claim(&self, block_number: u32) {
        let prev = IoBlockRef(self.io_refcounts[block_number as usize].swap(0, Ordering::Release));
        assert_eq!(
            prev,
            IoBlockRef::TRIM_CLAIMED,
            "release_trim_claim on block {block_number} that wasn't claimed (was {:#06x})",
            prev.0,
        );
        self.refcount_event.notify(usize::MAX);
    }

    /// Load the current raw I/O refcount for a block (for testing).
    #[cfg(test)]
    pub(crate) fn io_refcount(&self, block_number: u32) -> u16 {
        self.io_refcounts[block_number as usize].load(Ordering::Acquire)
    }

    /// Compute the BAT entry index for a given data block number.
    ///
    /// For every `chunk_ratio` payload entries, one sector bitmap entry is
    /// interleaved. The entry index accounts for these interleaved entries.
    pub fn payload_entry_index(&self, block_number: u32) -> u32 {
        block_number + (block_number / self.chunk_ratio)
    }

    /// Compute the BAT entry index for a given sector bitmap block (chunk number).
    ///
    /// The sector bitmap entry follows every `chunk_ratio` payload entries.
    pub fn sector_bitmap_entry_index(&self, chunk_number: u32) -> u32 {
        ((chunk_number + 1) * self.chunk_ratio) + chunk_number
    }

    /// Reverse-map a flat BAT entry number to (block_type, block_number).
    ///
    /// Returns `None` if the entry is beyond the end of the disk.
    fn entry_number_to_block_id(&self, entry_number: u32) -> Option<(BlockType, u32)> {
        let group_size = self.chunk_ratio + 1;
        let group = entry_number / group_size;
        let position = entry_number % group_size;

        if position == self.chunk_ratio {
            // This is a sector bitmap / padding entry.
            if self.has_parent && group < self.sector_bitmap_block_count {
                Some((BlockType::SectorBitmap, group))
            } else {
                None
            }
        } else {
            // This is a payload entry.
            let block_number = group * self.chunk_ratio + position;
            if block_number < self.data_block_count {
                Some((BlockType::Payload, block_number))
            } else {
                None
            }
        }
    }

    /// Convert a virtual disk byte offset to a block number.
    pub fn offset_to_block(&self, offset: u64) -> u32 {
        (offset / self.block_size as u64) as u32
    }

    /// Compute the byte offset within a block for a given virtual disk offset.
    #[cfg(test)]
    pub fn offset_within_block(&self, offset: u64) -> u32 {
        (offset % self.block_size as u64) as u32
    }

    /// Iterate over the block spans touched by a virtual disk range.
    ///
    /// Given a guest `offset` and `len`, yields one [`BlockSpan`] per
    /// block touched, with the block-relative offset and clamped length.
    /// This is the single source of truth for the block-walk arithmetic
    /// used by read, write, complete, and abort paths.
    pub fn block_spans(&self, offset: u64, len: u32) -> BlockSpanIter {
        BlockSpanIter {
            block_size: self.block_size,
            base_offset: offset,
            total_len: len,
            current_offset: 0,
        }
    }

    /// Serialize a BAT page from in-memory state.
    ///
    /// Produces all entries for the given page, with TFP blocks having
    /// their `file_offset_mb` masked to zero (allocation not committed
    /// yet).
    fn produce_page(&self, page_index: usize, buf: &mut [u8; CACHE_PAGE_SIZE as usize]) {
        let base_entry = page_index as u32 * ENTRIES_PER_BAT_PAGE as u32;
        for i in 0..ENTRIES_PER_BAT_PAGE as u32 {
            let entry_number = base_entry + i;
            let bat_entry = match self.entry_number_to_block_id(entry_number) {
                Some((BlockType::Payload, block_number)) => {
                    let mapping = self.get_block_mapping(block_number);
                    let file_mb = if mapping.transitioning_to_fully_present() {
                        0
                    } else {
                        mapping.file_megabyte() as u64
                    };
                    BatEntry::new()
                        .with_state(mapping.state())
                        .with_file_offset_mb(file_mb)
                }
                Some((BlockType::SectorBitmap, chunk_number)) => {
                    let mapping = self.get_sector_bitmap_mapping(chunk_number);
                    BatEntry::new()
                        .with_state(mapping.state())
                        .with_file_offset_mb(mapping.file_megabyte() as u64)
                }
                None => BatEntry::new(),
            };
            let offset = i as usize * size_of::<BatEntry>();
            buf[offset..offset + size_of::<BatEntry>()].copy_from_slice(bat_entry.as_bytes());
        }
    }

    /// Write a block mapping to the cache, converting from in-memory
    /// representation to on-disk BAT entry format.
    ///
    /// Atomically updates the in-memory BAT and the cache page under
    /// the page lock, ensuring no window where the in-memory state is
    /// visible but the cache page hasn't been stamped with the FSN.
    ///
    /// Uses `Overwrite` mode to avoid unnecessary disk reads. If the
    /// page is already cached, patches only the single entry. If not
    /// cached, builds the full page from in-memory state (no disk read).
    pub async fn write_block_mapping<F: AsyncFile>(
        &self,
        cache: &PageCache<F>,
        block_type: BlockType,
        block_number: u32,
        mapping: BlockMapping,
        pre_log_fsn: Option<Fsn>,
    ) -> Result<(), VhdxIoError> {
        let entry_number = match block_type {
            BlockType::Payload => self.payload_entry_index(block_number),
            BlockType::SectorBitmap => self.sector_bitmap_entry_index(block_number),
        };
        let page_number = entry_number as usize / ENTRIES_PER_BAT_PAGE as usize;
        let page_offset = page_number as u64 * CACHE_PAGE_SIZE;
        let entry_within_page = entry_number as usize % ENTRIES_PER_BAT_PAGE as usize;

        let mut guard = cache
            .acquire_write(
                PageKey {
                    tag: BAT_TAG,
                    offset: page_offset,
                },
                WriteMode::Overwrite,
            )
            .await
            .map_err(VhdxIoErrorInner::BatCache)?;

        // Update in-memory BAT under the page lock. This ensures a
        // concurrent trim on a block sharing the same page can't dirty
        // the page (and get it flushed to WAL) between our in-memory
        // update and the FSN stamp below.
        match block_type {
            BlockType::Payload => {
                self.set_block_mapping(block_number, mapping);
            }
            BlockType::SectorBitmap => {
                self.set_sector_bitmap_mapping(block_number, mapping);
            }
        }

        if guard.is_overwriting() {
            // Slow path: page not cached — build from in-memory state.
            self.produce_page(page_number, &mut guard);
        } else {
            // Fast path: page is cached — patch just the one entry.
            let bat_entry = BatEntry::new()
                .with_state(mapping.state())
                .with_file_offset_mb(mapping.file_megabyte() as u64);
            let byte_offset = entry_within_page * size_of::<BatEntry>();
            guard[byte_offset..byte_offset + size_of::<BatEntry>()]
                .copy_from_slice(bat_entry.as_bytes());
        }

        // Set pre-log FSN while the page lock is still held, so
        // that the FSN is visible atomically with the dirty-mark.
        if let Some(fsn) = pre_log_fsn {
            guard.set_pre_log_fsn(fsn);
        }

        // BAT pages are always rebuildable from in-memory BatState,
        // so prefer evicting them over sector bitmap pages.
        guard.demote();

        Ok(())
    }

    /// Read chunk size for BAT loading (256 KiB = 32768 entries).
    const BAT_READ_CHUNK: usize = 256 * 1024;

    /// Load the in-memory BAT state from disk.
    ///
    /// Reads the BAT region in fixed-size chunks and does a single
    /// sequential pass over all entries, dispatching payload vs. SBM
    /// entries via [`entry_number_to_block_id`]. This avoids both a
    /// large peak allocation and redundant reads of the same region.
    ///
    /// During parse, marks allocated blocks in the FreeSpaceTracker
    /// and records soft-anchored blocks.
    pub(crate) async fn load_bat_state<F: AsyncFile>(
        &mut self,
        file: &F,
        bat_offset: u64,
        bat_length: u32,
        free_space: &FreeSpaceTracker,
        eof_state: &mut EofState,
    ) -> Result<(), OpenError> {
        let bat_len = bat_length as usize;
        let total_entries = bat_len / size_of::<BatEntry>();
        let chunk_size = std::cmp::min(bat_len, Self::BAT_READ_CHUNK);
        let entries_per_chunk = chunk_size / size_of::<BatEntry>();

        let mut file_pos: usize = 0;
        let mut entry_num = 0;

        // Allocate a single read buffer, reused across iterations.
        // On the last iteration, we may read up to `chunk_size` bytes
        // even if fewer remain — the excess is zero-initialized and
        // ignored by the parser.
        let mut buf = file.alloc_buffer(chunk_size);

        while entry_num < total_entries {
            // Read the next chunk.
            buf = file
                .read_into(bat_offset + file_pos as u64, buf)
                .await
                .map_err(OpenErrorInner::Io)?;

            let entries_in_chunk = std::cmp::min(entries_per_chunk, total_entries - entry_num);
            for i in 0..entries_in_chunk {
                let byte_offset = i * size_of::<BatEntry>();
                let entry = BatEntry::read_from_bytes(
                    &buf.as_ref()[byte_offset..byte_offset + size_of::<BatEntry>()],
                )
                .map_err(|_| CorruptionType::InvalidBlockState)?;

                match self.entry_number_to_block_id((entry_num + i) as u32) {
                    Some((BlockType::Payload, block_number)) => {
                        let mapping = BlockMapping::from_bat_entry(entry, self.has_parent)?;
                        if mapping.bat_state().is_allocated() {
                            let file_offset = mapping.file_offset();
                            if file_offset != 0 {
                                free_space.mark_range_in_use(
                                    eof_state,
                                    file_offset,
                                    self.block_size,
                                )?;
                            }
                        } else if (mapping.bat_state() == BatEntryState::Unmapped
                            || mapping.bat_state() == BatEntryState::Undefined)
                            && mapping.file_megabyte() != 0
                        {
                            let file_offset = mapping.file_offset();
                            free_space.mark_range_in_use(
                                eof_state,
                                file_offset,
                                self.block_size,
                            )?;
                            free_space.mark_trimmed_block(
                                block_number,
                                file_offset,
                                self.block_size,
                            )?;
                        }
                        self.payload_mappings[block_number as usize]
                            .store(mapping.into(), Ordering::Relaxed);
                    }
                    Some((BlockType::SectorBitmap, chunk_number)) => {
                        let mapping = BlockMapping::from_sbm_bat_entry(entry)?;
                        if mapping.bat_state().is_allocated() {
                            let file_offset = mapping.file_offset();
                            if file_offset != 0 {
                                free_space.mark_range_in_use(
                                    eof_state,
                                    file_offset,
                                    SECTOR_BITMAP_BLOCK_SIZE,
                                )?;
                            }
                        }
                        self.sector_bitmap_mappings[chunk_number as usize]
                            .store(mapping.into(), Ordering::Relaxed);
                    }
                    None => {
                        // Entry beyond the disk — padding per the VHDX spec.
                    }
                }
            }

            entry_num += entries_in_chunk;
            file_pos += chunk_size;
        }

        // Cross-validate: every PartiallyPresent payload block must have
        // a corresponding allocated (FullyPresent) SBM block. This is
        // required by the VHDX spec and enforced at runtime by
        // ensure_sbm_allocated, but a corrupt file could violate it.
        if self.has_parent {
            for block in 0..self.data_block_count {
                let mapping = BlockMapping::from(
                    self.payload_mappings[block as usize].load(Ordering::Relaxed),
                );
                if mapping.bat_state() == BatEntryState::PartiallyPresent {
                    let chunk = block / self.chunk_ratio;
                    let sbm = BlockMapping::from(
                        self.sector_bitmap_mappings[chunk as usize].load(Ordering::Relaxed),
                    );
                    if sbm.bat_state() != BatEntryState::FullyPresent {
                        return Err(CorruptionType::PartiallyPresentWithoutSectorBitmap.into());
                    }
                }
            }
        }

        Ok(())
    }

    /// Atomically increment I/O refcounts for a contiguous range of
    /// blocks, returning a [`BatGuard`] that releases them on drop.
    ///
    /// Blocks are acquired in ascending order. If a block is claimed
    /// by trim, the caller holds previously-acquired blocks and waits
    /// for the blocked block to become available. Deadlock-free because
    /// both I/O and trim always acquire blocks in ascending order.
    pub async fn acquire_io_refcounts(&self, start_block: u32, block_count: u32) -> BatGuard<'_> {
        let mut guard = BatGuard {
            bat: Some(self),
            start_block,
            block_count: 0,
        };
        for block in start_block..start_block + block_count {
            while !self.try_increment_io_refcount(block) {
                let listener = self.refcount_event.listen();
                if !self.try_increment_io_refcount(block) {
                    listener.await;
                }
            }
            guard.block_count += 1;
        }
        guard
    }

    /// Look up the payload block mapping for a given data block number.
    ///
    /// Returns a point-in-time snapshot. Callers that hold I/O
    /// refcounts can rely on the following:
    ///
    /// - **`file_offset` is stable for allocated blocks.** If the
    ///   mapping shows `FullyPresent` or `PartiallyPresent`, the file
    ///   offset won't be reclaimed out from under you — trim must
    ///   drain I/O refcounts before it can claim the block.
    /// - **State can only advance, not regress.** A block that is
    ///   `FullyPresent` won't revert to `NotPresent` while I/O
    ///   refcounts are held. (Trim sets the pending bit to block new
    ///   I/O, then waits for existing I/O to drain.)
    /// - **TFP blocks are in flight.** If `transitioning_to_fully_present`
    ///   is set, another writer is mid-allocation. The write path waits
    ///   on `allocation_event` and retries; the read path ignores TFP
    ///   and uses the current state+offset directly (safe because the
    ///   file offset is valid and I/O refcounts prevent reclamation).
    ///
    /// Without I/O refcounts (or a trim claim), the mapping is purely
    /// advisory — the block could be trimmed between the load and any
    /// action on it.
    pub(crate) fn get_block_mapping(&self, block_number: u32) -> BlockMapping {
        BlockMapping::from(self.payload_mappings[block_number as usize].load(Ordering::Acquire))
    }

    /// Look up the sector bitmap block mapping for a given chunk number.
    ///
    /// SBM mappings are set once during allocation and never revert,
    /// so any reader that sees `FullyPresent` can rely on the file
    /// offset being stable indefinitely. A reader that sees
    /// `NotPresent` must allocate the SBM block before proceeding
    /// (see `ensure_sbm_allocated`).
    pub(crate) fn get_sector_bitmap_mapping(&self, chunk_number: u32) -> BlockMapping {
        BlockMapping::from(
            self.sector_bitmap_mappings[chunk_number as usize].load(Ordering::Acquire),
        )
    }

    /// Update the payload block mapping for a given data block number.
    ///
    /// In-memory only — does not persist to cache or disk. Use
    /// [`write_block_mapping`](Self::write_block_mapping) to persist.
    ///
    /// Allowed transitions and their required guards:
    ///
    /// - Unallocated → same state + TFP + file offset: `allocation_lock`.
    ///   (Unallocated = NotPresent, Zero, Unmapped, or Undefined.)
    /// - PartiallyPresent → same state + TFP: `allocation_lock`.
    /// - Any + TFP → original mapping (revert): abort path — TFP
    ///   acts as an exclusive flag so no other guard is needed.
    /// - Soft-anchored → same state + file_megabyte=0: `allocation_lock`.
    ///
    /// The TFP bit is the key invariant: once set on a block, no other
    /// allocator will touch that block (they wait on `allocation_event`),
    /// and trim cannot reach it because the allocator holds I/O
    /// refcounts on TFP blocks. This makes the setter the exclusive
    /// owner until TFP is cleared.
    pub(crate) fn set_block_mapping(&self, block_number: u32, mapping: BlockMapping) {
        self.payload_mappings[block_number as usize].store(mapping.into(), Ordering::Release);
    }

    /// Update the sector bitmap block mapping for a given chunk number.
    ///
    /// Only called from [`write_block_mapping`](Self::write_block_mapping)
    /// under the page cache write lock. SBM mappings transition from
    /// `NotPresent` to `FullyPresent` exactly once and never revert.
    fn set_sector_bitmap_mapping(&self, chunk_number: u32, mapping: BlockMapping) {
        self.sector_bitmap_mappings[chunk_number as usize].store(mapping.into(), Ordering::Release);
    }

    /// Initialize payload mappings for testing. Replaces any existing
    /// mappings with `data_block_count` entries set to `NotPresent`.
    #[cfg(test)]
    pub(crate) fn init_test_payload_mappings(&mut self) {
        let not_present = BlockMapping::new().with_bat_state(BatEntryState::NotPresent);
        for mapping in &self.payload_mappings {
            mapping.store(not_present.into(), Ordering::Relaxed);
        }
    }
}

#[must_use]
pub struct BatGuard<'a> {
    bat: Option<&'a Bat>,
    /// First payload block number with incremented refcount.
    start_block: u32,
    /// Number of consecutive payload blocks with incremented refcounts.
    block_count: u32,
}

impl<'a> BatGuard<'a> {
    pub(crate) fn empty() -> Self {
        Self {
            bat: None,
            start_block: 0,
            block_count: 0,
        }
    }
}

impl Drop for BatGuard<'_> {
    fn drop(&mut self) {
        let Some(bat) = self.bat else { return };
        let mut notify = false;
        for block in self.start_block..self.start_block + self.block_count {
            notify |= bat.decrement_io_refcount(block);
        }
        if notify {
            bat.refcount_event.notify(usize::MAX);
        }
    }
}

#[must_use]
pub struct TrimGuard<'a> {
    bat: &'a Bat,
    block_number: u32,
}

impl Drop for TrimGuard<'_> {
    fn drop(&mut self) {
        self.bat.release_trim_claim(self.block_number);
    }
}

/// A single block's portion of a virtual disk I/O range.
///
/// Produced by [`Bat::block_spans`]. Each span describes one block's
/// contribution to an `(offset, len)` range.
#[derive(Debug, Clone, Copy)]
pub struct BlockSpan {
    /// Block number within the BAT.
    pub block_number: u32,
    /// Byte offset within the block where this span starts.
    pub block_offset: u32,
    /// Number of bytes this span covers within the block.
    pub length: u32,
    /// Absolute guest virtual disk byte offset for this span.
    pub virtual_offset: u64,
}

impl BlockSpan {
    /// Whether this span covers the entire block.
    pub fn is_full_block(&self, block_size: u32) -> bool {
        self.block_offset == 0 && self.length >= block_size
    }
}

/// Iterator over [`BlockSpan`]s produced by [`Bat::block_spans`].
pub struct BlockSpanIter {
    block_size: u32,
    base_offset: u64,
    total_len: u32,
    current_offset: u32,
}

impl Iterator for BlockSpanIter {
    type Item = BlockSpan;

    fn next(&mut self) -> Option<BlockSpan> {
        if self.current_offset >= self.total_len {
            return None;
        }
        let virtual_offset = self.base_offset + self.current_offset as u64;
        let block_number = (virtual_offset / self.block_size as u64) as u32;
        let block_offset = (virtual_offset % self.block_size as u64) as u32;
        let length = std::cmp::min(
            self.block_size - block_offset,
            self.total_len - self.current_offset,
        );
        self.current_offset += length;
        Some(BlockSpan {
            block_number,
            block_offset,
            length,
            virtual_offset,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format;
    use pal_async::async_test;
    use std::sync::Arc;

    #[test]
    fn chunk_ratio_default_params() {
        // 2 MiB blocks, 512-byte sectors → chunk_ratio = 2048
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
        assert_eq!(bat.chunk_ratio, 2048);
    }

    #[test]
    fn chunk_ratio_various_sizes() {
        // 1 MiB blocks, 512 sectors
        let bat = Bat::new(format::GB1, MB1 as u32, 512, false, MB1 as u32).unwrap();
        assert_eq!(bat.chunk_ratio, 4096);

        // 4 MiB blocks, 512 sectors
        let bat = Bat::new(format::GB1, 4 * MB1 as u32, 512, false, MB1 as u32).unwrap();
        assert_eq!(bat.chunk_ratio, 1024);

        // 32 MiB blocks, 512 sectors
        let bat = Bat::new(format::GB1, 32 * MB1 as u32, 512, false, MB1 as u32).unwrap();
        assert_eq!(bat.chunk_ratio, 128);

        // 256 MiB blocks, 512 sectors
        let bat = Bat::new(format::GB1, 256 * MB1 as u32, 512, false, MB1 as u32).unwrap();
        assert_eq!(bat.chunk_ratio, 16);

        // 2 MiB blocks, 4096 sectors: sectors_per_block = 512, chunk_ratio = 8388608 / 512 = 16384
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            4096,
            false,
            MB1 as u32,
        )
        .unwrap();
        assert_eq!(bat.chunk_ratio, 16384);

        // 1 MiB blocks, 4096 sectors: sectors_per_block = 256, chunk_ratio = 8388608 / 256 = 32768
        let bat = Bat::new(format::GB1, MB1 as u32, 4096, false, MB1 as u32).unwrap();
        assert_eq!(bat.chunk_ratio, 32768);
    }

    #[test]
    fn payload_entry_index_calculations() {
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
        // chunk_ratio = 2048
        assert_eq!(bat.payload_entry_index(0), 0);
        assert_eq!(bat.payload_entry_index(1), 1);
        assert_eq!(
            bat.payload_entry_index(bat.chunk_ratio - 1),
            bat.chunk_ratio - 1
        );
        // At chunk_ratio, we skip one SBM slot.
        assert_eq!(
            bat.payload_entry_index(bat.chunk_ratio),
            bat.chunk_ratio + 1
        );
        assert_eq!(
            bat.payload_entry_index(bat.chunk_ratio + 1),
            bat.chunk_ratio + 2
        );
        // At 2 * chunk_ratio, skip another.
        assert_eq!(
            bat.payload_entry_index(2 * bat.chunk_ratio),
            2 * bat.chunk_ratio + 2
        );
    }

    #[test]
    fn sector_bitmap_entry_index_calculations() {
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            true,
            MB1 as u32,
        )
        .unwrap();
        // SBM entry 0 is at position chunk_ratio.
        assert_eq!(bat.sector_bitmap_entry_index(0), bat.chunk_ratio);
        // SBM entry 1 is at position 2*chunk_ratio + 1.
        assert_eq!(bat.sector_bitmap_entry_index(1), 2 * bat.chunk_ratio + 1);
    }

    #[test]
    fn validate_bat_size_ok() {
        // For 1 GiB / 2 MiB = 512 data blocks, chunk_ratio = 2048.
        // entries = 512 + ((512-1)/2048) = 512 + 0 = 512
        // 512 * 8 = 4096 bytes. Any bat_length >= 4096 is fine.
        Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
    }

    #[test]
    fn validate_bat_size_too_small() {
        // 512 entries * 8 bytes = 4096 bytes needed.
        let result = Bat::new(format::GB1, format::DEFAULT_BLOCK_SIZE, 512, false, 4095);
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::BatTooSmall
            )))
        ));
    }

    #[test]
    fn offset_to_block_calculations() {
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
        assert_eq!(bat.offset_to_block(0), 0);
        assert_eq!(
            bat.offset_to_block(format::DEFAULT_BLOCK_SIZE as u64 - 1),
            0
        );
        assert_eq!(bat.offset_to_block(format::DEFAULT_BLOCK_SIZE as u64), 1);
        assert_eq!(
            bat.offset_to_block(format::DEFAULT_BLOCK_SIZE as u64 * 10 + 42),
            10
        );
    }

    #[test]
    fn offset_within_block_calculations() {
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
        assert_eq!(bat.offset_within_block(0), 0);
        assert_eq!(bat.offset_within_block(512), 512);
        assert_eq!(
            bat.offset_within_block(format::DEFAULT_BLOCK_SIZE as u64),
            0
        );
        assert_eq!(
            bat.offset_within_block(format::DEFAULT_BLOCK_SIZE as u64 + 1024),
            1024
        );
    }

    #[test]
    fn mapping_max_file_megabyte() {
        let max_mb: u32 = (1 << 28) - 1; // 268435455
        let mapping = BlockMapping::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_megabyte(max_mb);
        assert_eq!(mapping.file_megabyte(), max_mb);
        assert_eq!(mapping.file_offset(), max_mb as u64 * MB1);
    }

    #[test]
    fn mapping_tfp_flag() {
        let with_tfp = BlockMapping::new()
            .with_state(BatEntryState::NotPresent as u8)
            .with_transitioning_to_fully_present(true);
        assert!(with_tfp.transitioning_to_fully_present());

        let without_tfp = BlockMapping::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_transitioning_to_fully_present(false);
        assert!(!without_tfp.transitioning_to_fully_present());

        // TFP is independent of state.
        assert_eq!(with_tfp.state(), BatEntryState::NotPresent as u8);
        assert_eq!(without_tfp.state(), BatEntryState::FullyPresent as u8);
    }

    #[test]
    fn mapping_from_bat_entry() {
        let entry = BatEntry::new()
            .with_state(BatEntryState::FullyPresent as u8)
            .with_file_offset_mb(100);
        let mapping = BlockMapping::from_bat_entry(entry, false).unwrap();
        assert_eq!(mapping.state(), BatEntryState::FullyPresent as u8);
        assert_eq!(mapping.file_megabyte(), 100);
        assert!(!mapping.transitioning_to_fully_present());
    }

    #[test]
    fn mapping_rejects_reserved_bat_entry_bits() {
        let entry = BatEntry::from(
            u64::from(BatEntry::new().with_state(BatEntryState::FullyPresent as u8)) | (1 << 3),
        );
        let result = BlockMapping::from_bat_entry(entry, false);
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::ReservedBatEntryFieldNonzero
            )))
        ));
    }

    #[test]
    fn entry_number_to_block_id_payload() {
        // Non-differencing: all entries are payload.
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
        // chunk_ratio = 2048, data_block_count = 512
        for i in 0..bat.data_block_count {
            let entry_index = bat.payload_entry_index(i);
            let result = bat.entry_number_to_block_id(entry_index);
            assert_eq!(result, Some((BlockType::Payload, i)), "block {i}");
        }
    }

    #[test]
    fn entry_number_to_block_id_with_sbm() {
        // Differencing disk with SBM entries.
        // Use small chunk_ratio to exercise interleaving.
        // 1 MiB blocks, 4096 sectors → chunk_ratio = 32768.
        // Use 256 MiB blocks, 512 sectors → chunk_ratio = 16.
        let bat = Bat::new(format::GB1, 256 * MB1 as u32, 512, true, MB1 as u32).unwrap();
        assert_eq!(bat.chunk_ratio, 16);
        // data_block_count = 4, sector_bitmap_block_count = 1

        // Payload entries for group 0: positions 0..15 → blocks 0..3
        for i in 0..bat.data_block_count {
            let entry_index = bat.payload_entry_index(i);
            let result = bat.entry_number_to_block_id(entry_index);
            assert_eq!(result, Some((BlockType::Payload, i)), "payload block {i}");
        }

        // SBM entry for chunk 0 at position chunk_ratio = 16
        let sbm_index = bat.sector_bitmap_entry_index(0);
        assert_eq!(
            bat.entry_number_to_block_id(sbm_index),
            Some((BlockType::SectorBitmap, 0))
        );
    }

    #[test]
    fn entry_number_to_block_id_beyond_end() {
        let bat = Bat::new(
            format::GB1,
            format::DEFAULT_BLOCK_SIZE,
            512,
            false,
            MB1 as u32,
        )
        .unwrap();
        // Entry beyond all data blocks should return None.
        let beyond = bat.payload_entry_index(bat.data_block_count);
        assert_eq!(bat.entry_number_to_block_id(beyond), None);
    }

    /// Non-differencing disk with data_block_count > chunk_ratio.
    ///
    /// The BAT has padding entries at every chunk_ratio boundary. These
    /// must NOT be misidentified as payload entries.
    #[test]
    fn entry_number_to_block_id_padding_not_payload() {
        // Use 256 MiB blocks so chunk_ratio is small (16 with 512B sectors).
        // 8 GiB disk → data_block_count = 32 (> chunk_ratio=16).
        let bat = Bat::new(
            8 * format::GB1,
            256 * MB1 as u32,
            512,
            false,
            4 * MB1 as u32, // BAT length large enough
        )
        .unwrap();
        assert_eq!(bat.chunk_ratio, 16);
        assert_eq!(bat.data_block_count, 32);

        // Entry 16 is the padding entry (position == chunk_ratio in group 0).
        // It should NOT map to payload block 16.
        let padding_entry = bat.chunk_ratio; // entry 16
        let result = bat.entry_number_to_block_id(padding_entry);
        assert_eq!(
            result, None,
            "entry {} is a padding entry on non-diff disk and should return None, \
             but got {:?}",
            padding_entry, result
        );

        // Payload block 16 should be at entry 17 (payload_entry_index(16) = 16 + 16/16 = 17).
        let real_entry = bat.payload_entry_index(16);
        assert_eq!(real_entry, 17);
        let result = bat.entry_number_to_block_id(real_entry);
        assert_eq!(
            result,
            Some((BlockType::Payload, 16)),
            "entry {} should map to payload block 16",
            real_entry
        );
    }

    // ---- Refcount async behavior tests ----

    fn make_test_bat() -> Bat {
        Bat::new(4 * MB1, format::DEFAULT_BLOCK_SIZE, 512, false, MB1 as u32).unwrap()
    }

    #[test]
    fn decrement_preserves_trim_pending_bit() {
        let bat = make_test_bat();
        // Simulate: trim-pending with 3 in-flight I/Os draining.
        bat.io_refcounts[0].store(IoBlockRef::TRIM_PENDING_BIT | 3, Ordering::Release);
        assert!(!bat.decrement_io_refcount(0), "3→2 should not need notify");
        // After decrement: pending bit preserved, count is 2.
        let cur = IoBlockRef(bat.io_refcount(0));
        assert!(cur.trim_pending());
        assert_eq!(cur.io_count(), 2);
    }

    #[test]
    #[should_panic(expected = "io_refcount underflow")]
    fn decrement_panics_on_underflow() {
        let _ = make_test_bat().decrement_io_refcount(0);
    }

    #[test]
    #[should_panic(expected = "trim claimed")]
    fn decrement_panics_on_trim_claimed() {
        let bat = make_test_bat();
        bat.io_refcounts[0].store(IoBlockRef::TRIM_CLAIMED.0, Ordering::Release);
        let _ = bat.decrement_io_refcount(0);
    }

    #[test]
    #[should_panic(expected = "wasn't claimed")]
    fn release_trim_claim_panics_if_not_claimed() {
        make_test_bat().release_trim_claim(0);
    }

    #[async_test]
    async fn acquire_io_on_idle_block() {
        let bat = make_test_bat();
        let guard = bat.acquire_io_refcounts(0, 1).await;
        assert_eq!(bat.io_refcount(0), 1);
        drop(guard);
        assert_eq!(bat.io_refcount(0), 0);
    }

    #[async_test]
    async fn acquire_io_resumes_after_trim_releases() {
        let bat = Arc::new(make_test_bat());
        bat.io_refcounts[0].store(IoBlockRef::TRIM_CLAIMED.0, Ordering::Release);

        let bat2 = bat.clone();
        let io_task = async move {
            let guard = bat2.acquire_io_refcounts(0, 1).await;
            assert_eq!(bat2.io_refcount(0), 1);
            drop(guard);
        };

        let release_task = async {
            bat.release_trim_claim(0);
        };

        futures::future::join(io_task, release_task).await;
        assert_eq!(bat.io_refcount(0), 0);
    }

    #[async_test]
    async fn acquire_io_multi_block_rolls_back_on_partial_conflict() {
        let bat = Arc::new(make_test_bat());
        bat.io_refcounts[1].store(IoBlockRef::TRIM_CLAIMED.0, Ordering::Release);

        let bat2 = bat.clone();
        let io_task = async move {
            let guard = bat2.acquire_io_refcounts(0, 2).await;
            assert_eq!(bat2.io_refcount(0), 1);
            assert_eq!(bat2.io_refcount(1), 1);
            drop(guard);
        };

        let release_task = async {
            bat.release_trim_claim(1);
        };

        futures::future::join(io_task, release_task).await;
    }

    #[async_test]
    async fn claim_for_trim_on_idle_block() {
        let bat = make_test_bat();
        let guard = bat.claim_for_trim(0).await;
        assert_eq!(bat.io_refcount(0), IoBlockRef::TRIM_CLAIMED.0);
        drop(guard);
        assert_eq!(bat.io_refcount(0), 0);
    }

    #[async_test]
    async fn claim_for_trim_waits_for_io_drain() {
        let bat = Arc::new(make_test_bat());

        let io_guard = bat.acquire_io_refcounts(0, 1).await;
        assert_eq!(bat.io_refcount(0), 1);

        let trim_task = async {
            let guard = bat.claim_for_trim(0).await;
            assert_eq!(bat.io_refcount(0), IoBlockRef::TRIM_CLAIMED.0);
            guard
        };

        let drain_task = async {
            // After trim_task's first poll, trim-pending is set.
            assert!(IoBlockRef(bat.io_refcount(0)).trim_pending());
            assert!(!bat.try_increment_io_refcount(0));
            drop(io_guard);
        };

        let (trim_guard, ()) = futures::future::join(trim_task, drain_task).await;
        drop(trim_guard);
        assert_eq!(bat.io_refcount(0), 0);
    }

    #[async_test]
    async fn trim_has_writer_priority_over_new_io() {
        let bat = Arc::new(make_test_bat());

        // Block 0 has an in-flight I/O.
        let io_guard = bat.acquire_io_refcounts(0, 1).await;

        // Trim claims — sets pending, waits for drain.
        let trim_task = async {
            let guard = bat.claim_for_trim(0).await;
            assert_eq!(bat.io_refcount(0), IoBlockRef::TRIM_CLAIMED.0);
            guard
        };
        let drain_task = async { drop(io_guard) };

        let (trim_guard, ()) = futures::future::join(trim_task, drain_task).await;

        // Trim owns the block. New I/O should be blocked.
        assert!(!bat.try_increment_io_refcount(0));

        // Release trim, then new I/O should succeed.
        drop(trim_guard);
        let io_guard2 = bat.acquire_io_refcounts(0, 1).await;
        assert_eq!(bat.io_refcount(0), 1);
        drop(io_guard2);
        assert_eq!(bat.io_refcount(0), 0);
    }

    #[async_test]
    async fn acquire_io_blocked_at_overflow_resumes() {
        let bat = Arc::new(make_test_bat());
        bat.io_refcounts[0].store(IoBlockRef::MAX_IO_REFCOUNT, Ordering::Release);

        let bat2 = bat.clone();
        let io_task = async move {
            let guard = bat2.acquire_io_refcounts(0, 1).await;
            assert_eq!(bat2.io_refcount(0), IoBlockRef::MAX_IO_REFCOUNT);
            drop(guard);
        };

        let unblock_task = async {
            if bat.decrement_io_refcount(0) {
                bat.refcount_event.notify(usize::MAX);
            }
        };

        futures::future::join(io_task, unblock_task).await;
    }
}
