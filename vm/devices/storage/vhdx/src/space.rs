// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Free space management for VHDX files.
//!
//! Tracks which megabyte-granularity regions of the file are free, in-use,
//! or soft-anchored (from trimmed blocks). Implements a four-priority
//! allocation strategy:
//!
//! 1. **Free space pool** — reuse interior free blocks
//! 2. **Near-EOF space** — allocate from zeroed space before file end
//! 3. **Soft-anchored blocks** — reclaim trimmed blocks (in-memory only)
//! 4. **Extend EOF** — grow the file
//!
//! The bitmap uses 1-bit-per-megabyte granularity with SET = free / anchored
//! and CLEAR = in-use.

use crate::bat::Bat;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::error::OpenErrorInner;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::format::BatEntryState;
use crate::format::MB1;
use bitfield_struct::bitfield;
use bitvec::prelude::*;
use parking_lot::Mutex;
use std::collections::HashMap;

/// Default EOF extension length: 32 MiB.
const DEFAULT_EOF_EXTENSION_LENGTH: u32 = 32 * MB1 as u32;

// ---------------------------------------------------------------------------
// SpaceBitmap — RTL_BITMAP equivalent
// ---------------------------------------------------------------------------

/// Bitmap wrapper providing `RTL_BITMAP`-equivalent operations.
///
/// Uses [`BitVec`] with LSB-first bit ordering on `u64` words for
/// word-level accelerated operations.
/// SET bits (1) denote the property tracked by the containing structure
/// (free, anchored, or trimmed); CLEAR bits (0) denote the opposite.
#[derive(Clone)]
struct SpaceBitmap {
    bits: BitVec<u64, Lsb0>,
}

impl SpaceBitmap {
    /// Create a new bitmap with `bit_count` bits, all initially clear.
    fn new(bit_count: usize) -> Self {
        SpaceBitmap {
            bits: bitvec![u64, Lsb0; 0; bit_count],
        }
    }

    /// Number of valid bits.
    fn len(&self) -> usize {
        self.bits.len()
    }

    /// Set a single bit.
    fn set_bit(&mut self, index: usize) {
        self.bits.set(index, true);
    }

    /// Clear a single bit.
    fn clear_bit(&mut self, index: usize) {
        self.bits.set(index, false);
    }

    /// Check whether a single bit is set.
    fn check_bit(&self, index: usize) -> bool {
        self.bits[index]
    }

    /// Set a contiguous range of bits `[start..start+count)`.
    fn set_range(&mut self, start: usize, count: usize) {
        self.bits[start..start + count].fill(true);
    }

    /// Clear a contiguous range of bits `[start..start+count)`.
    fn clear_range(&mut self, start: usize, count: usize) {
        self.bits[start..start + count].fill(false);
    }

    /// Check whether all bits in `[start..start+count)` are set.
    fn are_bits_set(&self, start: usize, count: usize) -> bool {
        count == 0 || self.bits[start..start + count].all()
    }

    /// Check whether all bits in `[start..start+count)` are clear.
    fn are_bits_clear(&self, start: usize, count: usize) -> bool {
        count == 0 || self.bits[start..start + count].not_any()
    }

    /// Find the first contiguous run of `count` SET bits, starting the
    /// scan at `hint`. Returns `None` if no such run exists.
    ///
    /// Scans `[hint..len)` first, then `[0..hint)`. Uses word-level
    /// `first_one` / `first_zero` operations for efficient run detection.
    fn find_set_bits(&self, count: usize, hint: usize) -> Option<usize> {
        let total = self.bits.len();
        if count == 0 || count > total {
            return None;
        }
        let hint = hint.min(total);

        // Pass 1: [hint..total)
        if let Some(idx) = Self::find_run(&self.bits, count, hint, total) {
            return Some(idx);
        }
        // Pass 2: [0..hint) — only the region not covered by pass 1.
        if hint > 0 {
            if let Some(idx) = Self::find_run(&self.bits, count, 0, hint) {
                return Some(idx);
            }
        }
        None
    }

    /// Set all valid bits.
    fn set_all(&mut self) {
        self.bits.fill(true);
    }

    /// Resize the bitmap to `new_bit_count`. New bits are cleared.
    /// Preserves existing data up to `min(old_count, new_count)`.
    fn resize(&mut self, new_bit_count: usize) {
        self.bits.resize(new_bit_count, false);
    }

    /// Find a contiguous run of `count` SET bits within `[start..end)`.
    fn find_run(
        bits: &BitSlice<u64, Lsb0>,
        count: usize,
        start: usize,
        end: usize,
    ) -> Option<usize> {
        if end - start < count {
            return None;
        }
        let window = &bits[start..end];
        let mut pos = 0;
        while pos + count <= window.len() {
            // Skip clear bits — find next set bit.
            let run_start = match window[pos..].first_one() {
                Some(i) => pos + i,
                None => return None,
            };
            if run_start + count > window.len() {
                return None;
            }
            // Find end of the set-bit run.
            let run_end = window[run_start..]
                .first_zero()
                .map_or(window.len(), |i| run_start + i);
            if run_end - run_start >= count {
                return Some(start + run_start);
            }
            pos = run_end;
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Sub-structures
// ---------------------------------------------------------------------------

/// Free space pool state. Tracks 1-bit-per-megabyte: SET = free.
struct FreeSpacePool {
    bitmap: SpaceBitmap,
    lowest_bit_hint: u32,
    /// Fast-path flag: if true, skip free-pool scan for block-sized allocations.
    no_free_blocks: bool,
}

/// Anchored space state. Tracks 1-bit-per-megabyte: SET = soft-anchored.
struct AnchoredSpacePool {
    bitmap: SpaceBitmap,
    lowest_bit_hint: u32,
}

/// Tracks which data blocks have been trimmed but still hold a
/// "soft anchor" to their file space.
///
/// When a block is trimmed with `TrimMode::FileSpace`, the BAT entry
/// transitions to Unmapped but the `file_megabyte` field is preserved.
/// The space is *not* released to the free pool. This avoids the cost
/// of zeroing + flushing the space before a future BAT commit, because
/// the space still contains only the block's own old data — no
/// cross-block data leak is possible on power failure.
///
/// Bitmap: 1-bit-per-block-number, SET = has soft-anchored file offset.
struct TrimmedBlockTracker {
    bitmap: SpaceBitmap,
    lowest_block_number_hint: u32,
    num_trimmed_blocks: u32,
}

/// EOF geometry state — describes where new space comes from.
///
/// These fields are only mutated under the `allocation_lock` (the async
/// `futures::lock::Mutex<()>` on `VhdxFile` that serializes the
/// allocate→TFP→write sequence). They live outside `FreeSpaceInner`
/// so they don't contend with the sync mutex.
pub(crate) struct EofState {
    /// Current file length (always MB1-aligned).
    pub file_length: u64,
    /// Highest in-use file offset.
    pub last_file_offset: u64,
    /// Offset at which all data beyond is guaranteed zero.
    pub zero_offset: u64,
    /// Minimum chunk for EOF extension (constant after init).
    pub eof_extension_length: u32,
}

/// Internal mutable state of the free space tracker.
struct FreeSpaceInner {
    free_space: FreeSpacePool,
    anchored_space: AnchoredSpacePool,
    trimmed_blocks: TrimmedBlockTracker,

    /// Block size in bytes.
    block_size: u32,
    /// Number of data blocks.
    data_block_count: u32,
}

// ---------------------------------------------------------------------------
// FreeSpaceTracker — public API
// ---------------------------------------------------------------------------

/// Free space tracker for VHDX files. All internal state is protected by
/// a synchronous `parking_lot::Mutex`.
///
/// This mutex must **never** be held across `.await` points. The outer
/// `allocation_lock` (an async mutex on `VhdxFile`) serializes the full
/// allocation sequence including any file I/O.
pub(crate) struct FreeSpaceTracker {
    inner: Mutex<FreeSpaceInner>,
    /// Block alignment (0 or power of 2 ≤ block_size). Constant after construction.
    block_alignment: u32,
}

/// Flags for [`VhdxFile::allocate_space()`].
#[bitfield(u8)]
#[derive(PartialEq, Eq)]
pub(crate) struct AllocateFlags {
    /// Align the allocation to `block_alignment`.
    #[bits(1)]
    pub aligned: bool,
    /// Zero the allocated region if not already zeroed on disk.
    #[bits(1)]
    pub zero: bool,
    #[bits(6)]
    _reserved: u8,
}

/// Describes the state of newly allocated space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpaceState {
    /// Fresh space from file extension — zeroed on disk. Safe to commit
    /// BAT before flushing the data write (no data leak possible).
    Zero,
    /// Recycled space containing the same block's own old data. Safe to
    /// commit BAT before flushing (a power failure only exposes the
    /// block's own stale data, not another block's). NOT zero.
    OwnStale,
    /// Recycled space that may contain another block's data. Must flush
    /// data writes before committing BAT to prevent cross-block data
    /// leaks on power failure. NOT zero.
    CrossStale,
}

impl SpaceState {
    /// Safe to commit BAT entry before data flush completes?
    pub fn is_safe(self) -> bool {
        matches!(self, Self::Zero | Self::OwnStale)
    }

    /// Guaranteed zeroed on disk?
    pub fn is_zero(self) -> bool {
        matches!(self, Self::Zero)
    }
}

/// Result from a successful space allocation.
pub(crate) struct AllocateResult {
    /// File byte offset of the allocated region.
    pub file_offset: u64,
    /// State of the allocated space.
    pub state: SpaceState,
    /// If this allocation reclaimed a cross-block soft anchor, the old
    /// block number whose `file_megabyte` must be cleared by the caller.
    pub unanchored_block: Option<u32>,
}

impl FreeSpaceTracker {
    /// Create and initialize the free space tracker.
    ///
    /// Called during `VhdxFile::open_inner()`, before the BAT parse. Sets all file
    /// space as free, then marks the header area, log, BAT, and metadata
    /// regions as in-use.
    ///
    /// Returns both the tracker and the initial [`EofState`].
    pub fn new(
        file_length: u64,
        block_size: u32,
        block_alignment: u32,
        header_area_size: u64,
        log_offset: u64,
        log_length: u32,
        bat_offset: u64,
        bat_length: u32,
        metadata_offset: u64,
        metadata_length: u32,
        data_block_count: u32,
    ) -> Result<(Self, EofState), OpenError> {
        // Validate alignment.
        if block_alignment != 0 && !block_alignment.is_power_of_two() {
            return Err(OpenErrorInner::InvalidParameter(
                crate::error::InvalidFormatReason::BlockAlignmentNotPowerOfTwo,
            )
            .into());
        }
        let effective_alignment = if block_alignment > block_size {
            0
        } else {
            block_alignment
        };
        // File length must be MB1-aligned.
        let aligned_file_length = (file_length + MB1 - 1) & !(MB1 - 1);
        let bit_count = (aligned_file_length / MB1) as usize;

        // Create bitmaps.
        let mut free_space_bitmap = SpaceBitmap::new(bit_count);
        let anchored_space_bitmap = SpaceBitmap::new(bit_count);
        let trimmed_block_bitmap = SpaceBitmap::new(data_block_count as usize);

        // Mark entire file as free.
        free_space_bitmap.set_all();

        let mut eof_state = EofState {
            file_length: aligned_file_length,
            last_file_offset: 0,
            zero_offset: 0,
            eof_extension_length: DEFAULT_EOF_EXTENSION_LENGTH,
        };

        let mut inner = FreeSpaceInner {
            free_space: FreeSpacePool {
                bitmap: free_space_bitmap,
                lowest_bit_hint: 0,
                no_free_blocks: false,
            },
            anchored_space: AnchoredSpacePool {
                bitmap: anchored_space_bitmap,
                lowest_bit_hint: bit_count as u32,
            },
            trimmed_blocks: TrimmedBlockTracker {
                bitmap: trimmed_block_bitmap,
                lowest_block_number_hint: data_block_count,
                num_trimmed_blocks: 0,
            },
            block_size,
            data_block_count,
        };

        // Mark header area as in-use.
        inner.mark_range_in_use_inner(&mut eof_state, 0, header_area_size as u32)?;

        // Mark log as in-use.
        if log_length > 0 {
            inner.mark_range_in_use_inner(&mut eof_state, log_offset, log_length)?;
        }

        // Mark BAT region as in-use.
        // BAT length is rounded up to MB1 for space tracking.
        let bat_length_aligned = round_up_mb1(bat_length as u64) as u32;
        inner.mark_range_in_use_inner(&mut eof_state, bat_offset, bat_length_aligned)?;

        // Mark metadata region as in-use.
        let metadata_length_aligned = round_up_mb1(metadata_length as u64) as u32;
        inner.mark_range_in_use_inner(&mut eof_state, metadata_offset, metadata_length_aligned)?;

        Ok((
            FreeSpaceTracker {
                inner: Mutex::new(inner),
                block_alignment: effective_alignment,
            },
            eof_state,
        ))
    }

    /// Block alignment (0 or power of 2). Constant after construction.
    pub fn block_alignment(&self) -> u32 {
        self.block_alignment
    }

    /// Mark a file range as in-use during BAT parse.
    ///
    /// Validates that the range doesn't overlap with an already-in-use range
    /// and doesn't extend past EOF.
    pub fn mark_range_in_use(
        &self,
        eof: &mut EofState,
        offset: u64,
        length: u32,
    ) -> Result<(), CorruptionType> {
        self.inner
            .lock()
            .mark_range_in_use_inner(eof, offset, length)
    }

    /// Mark a trimmed block as soft-anchored during BAT parse.
    pub fn mark_trimmed_block(
        &self,
        block_number: u32,
        file_offset: u64,
        block_size: u32,
    ) -> Result<(), CorruptionType> {
        self.inner
            .lock()
            .mark_trimmed_block_inner(block_number, file_offset, block_size)
    }

    /// Finalize after BAT parse. Separates EOF free space from pool free space.
    ///
    /// Blocks from `ZeroOffset` to `FileLength` are "near-EOF free space"
    /// (tracked separately, not in the bitmap pool). Clear those bits from
    /// the FreeSpace bitmap.
    pub fn complete_initialization(&self, eof: &EofState) {
        let mut inner = self.inner.lock();
        let bit_base = (eof.zero_offset / MB1) as usize;
        let bit_count = ((eof.file_length - eof.zero_offset) / MB1) as usize;
        if bit_count > 0 {
            debug_assert!(inner.free_space.bitmap.are_bits_set(bit_base, bit_count));
            inner.free_space.bitmap.clear_range(bit_base, bit_count);
        }
    }

    /// Try to allocate using priorities 1–3, with access to the BAT state
    /// for soft-anchor lookup (priority 3).
    pub fn try_allocate_with_bat(
        &self,
        eof: &mut EofState,
        size: u32,
        aligned: bool,
        bat: &Bat,
    ) -> Option<AllocateResult> {
        self.try_allocate_inner(eof, size, aligned, Some(bat))
    }

    /// Try all three in-memory allocation priorities.
    fn try_allocate_inner(
        &self,
        eof: &mut EofState,
        size: u32,
        aligned: bool,
        bat: Option<&Bat>,
    ) -> Option<AllocateResult> {
        let mut inner = self.inner.lock();
        // Priority 1: free space pool.
        if let Some(offset) = inner.free_space_pool_alloc(eof, size) {
            return Some(AllocateResult {
                file_offset: offset,
                state: SpaceState::CrossStale,
                unanchored_block: None,
            });
        }

        // Priority 2: near-EOF space (between ZeroOffset and FileLength).
        let aligned_zero_offset = if aligned && self.block_alignment != 0 {
            round_up(eof.zero_offset, self.block_alignment as u64)
        } else {
            eof.zero_offset
        };

        if eof.file_length >= aligned_zero_offset + size as u64 {
            let offset = aligned_zero_offset;
            eof.zero_offset = aligned_zero_offset + size as u64;
            eof.last_file_offset = eof.zero_offset;
            return Some(AllocateResult {
                file_offset: offset,
                state: SpaceState::Zero,
                unanchored_block: None,
            });
        }

        // Priority 3: soft-anchored space from trimmed blocks.
        //
        // Only considers blocks in TrimmedBlockTracker, which are populated
        // by flush() — so they are always durable. The caller must clear
        // the old block's file_megabyte in BatState and write its BAT page
        // to cache.
        if size <= inner.block_size {
            if let Some(bat) = bat {
                if let Some((file_offset, block_number)) =
                    inner.find_and_unanchor_in_memory_inner(bat)
                {
                    // If the allocated block is larger than needed, release excess.
                    if size < inner.block_size {
                        let excess_offset = file_offset + size as u64;
                        let excess_size = inner.block_size - size;
                        inner.release_inner(excess_offset, excess_size);
                    }
                    return Some(AllocateResult {
                        file_offset,
                        state: SpaceState::CrossStale,
                        unanchored_block: Some(block_number),
                    });
                }
            }
        }

        // Priority 4: caller must extend EOF.
        None
    }

    /// Release space back to the free pool.
    pub fn release(&self, offset: u64, size: u32) {
        self.inner.lock().release_inner(offset, size);
    }

    /// Unmark a trimmed block (when its space is reclaimed).
    #[must_use]
    pub fn unmark_trimmed_block(
        &self,
        block_number: u32,
        file_offset: u64,
        block_size: u32,
    ) -> bool {
        let mut inner = self.inner.lock();
        inner.unmark_trimmed_block_inner(block_number, file_offset, block_size)
    }

    /// Compute truncation target size.
    pub fn truncate_target(&self, eof: &EofState, is_fully_allocated: bool) -> u64 {
        let inner = self.inner.lock();
        let mut target = eof.last_file_offset;
        if is_fully_allocated {
            let excess = inner.compute_excess_block_count(eof, target);
            let extra = (excess as u64) * inner.block_size as u64;
            target = (target + extra).min(eof.file_length);
        }
        target
    }

    /// Update state after truncation.
    pub fn apply_truncate(&self, eof: &mut EofState, new_file_length: u64) {
        let mut inner = self.inner.lock();
        let aligned = (new_file_length + MB1 - 1) & !(MB1 - 1);
        let new_bit_count = (aligned / MB1) as usize;
        let old_bit_count = inner.free_space.bitmap.len();

        if new_bit_count < old_bit_count {
            inner.free_space.bitmap.resize(new_bit_count);
            inner.anchored_space.bitmap.resize(new_bit_count);
        }

        eof.file_length = aligned;
        eof.zero_offset = eof.zero_offset.min(aligned);
    }
}

impl EofState {
    /// Compute the target file size for EOF extension.
    ///
    /// Includes `eof_extension_length` minimum chunk.
    pub fn required_file_length(&self, block_alignment: u32, size: u32, aligned: bool) -> u64 {
        let aligned_zero_offset = if aligned && block_alignment != 0 {
            round_up(self.zero_offset, block_alignment as u64)
        } else {
            self.zero_offset
        };
        let target = aligned_zero_offset + size as u64;
        let min_target = self.file_length + self.eof_extension_length as u64;
        target.max(min_target)
    }

    /// Update state after file extension completed.
    ///
    /// Resizes bitmaps if needed and updates `file_length`.
    pub fn complete_file_extend(&mut self, tracker: &FreeSpaceTracker, new_file_length: u64) {
        let mut inner = tracker.inner.lock();
        let aligned = (new_file_length + MB1 - 1) & !(MB1 - 1);
        let new_bit_count = (aligned / MB1) as usize;
        let old_bit_count = inner.free_space.bitmap.len();

        if new_bit_count > old_bit_count {
            // Grow by at least 125% to avoid O(n²) behavior.
            let target_bits = (old_bit_count + old_bit_count / 4).max(new_bit_count);
            inner.free_space.bitmap.resize(target_bits);
            inner.anchored_space.bitmap.resize(target_bits);
        }

        self.file_length = aligned;
    }
}

// ---------------------------------------------------------------------------
// Internal helpers (operate on FreeSpaceInner, called under lock)
// ---------------------------------------------------------------------------

/// Round `value` up to the nearest multiple of `alignment`.
fn round_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}

/// Round `value` up to the nearest MB1 boundary.
fn round_up_mb1(value: u64) -> u64 {
    round_up(value, MB1)
}

impl FreeSpaceInner {
    /// Mark a file range as in-use during parse (internal, no lock).
    fn mark_range_in_use_inner(
        &mut self,
        eof: &mut EofState,
        offset: u64,
        length: u32,
    ) -> Result<(), CorruptionType> {
        debug_assert!(offset.is_multiple_of(MB1), "offset must be MB1-aligned");
        debug_assert!(
            (length as u64).is_multiple_of(MB1),
            "length must be MB1-aligned"
        );

        if length == 0 {
            return Ok(());
        }

        // Check range is within file.
        if eof.file_length < offset || eof.file_length - offset < length as u64 {
            return Err(CorruptionType::RangeBeyondEof);
        }

        let bit_base = (offset / MB1) as usize;
        let bit_count = length as usize / MB1 as usize;

        // Overlap check: all bits must currently be SET (free).
        if !self.free_space.bitmap.are_bits_set(bit_base, bit_count) {
            return Err(CorruptionType::RangeCollision);
        }

        // Mark as in-use (clear the bits).
        self.free_space.bitmap.clear_range(bit_base, bit_count);

        // Update last_file_offset and zero_offset.
        let range_end = offset + length as u64;
        if range_end > eof.last_file_offset {
            eof.last_file_offset = range_end;
            if eof.last_file_offset > eof.zero_offset {
                eof.zero_offset = eof.last_file_offset;
            }
        }

        Ok(())
    }

    /// Mark a trimmed block as soft-anchored (internal, no lock).
    fn mark_trimmed_block_inner(
        &mut self,
        block_number: u32,
        file_offset: u64,
        block_size: u32,
    ) -> Result<(), CorruptionType> {
        debug_assert!(block_number < self.data_block_count);
        debug_assert!(block_size.is_multiple_of(MB1 as u32));

        // Check: already marked as trimmed?
        if self.trimmed_blocks.bitmap.check_bit(block_number as usize) {
            return Err(CorruptionType::TrimmedRangeCollision);
        }

        // Check: anchored space bits must be clear (no collision with another anchor).
        let bit_base = (file_offset / MB1) as usize;
        let bit_count = block_size as usize / MB1 as usize;
        if !self
            .anchored_space
            .bitmap
            .are_bits_clear(bit_base, bit_count)
        {
            return Err(CorruptionType::TrimmedRangeCollision);
        }

        // Mark in trimmed block tracker.
        self.trimmed_blocks.bitmap.set_bit(block_number as usize);
        self.trimmed_blocks.num_trimmed_blocks += 1;
        self.trimmed_blocks.lowest_block_number_hint = self
            .trimmed_blocks
            .lowest_block_number_hint
            .min(block_number);

        // Mark in anchored space bitmap.
        self.anchored_space.bitmap.set_range(bit_base, bit_count);
        self.anchored_space.lowest_bit_hint =
            self.anchored_space.lowest_bit_hint.min(bit_base as u32);

        Ok(())
    }

    /// Unmark a trimmed block (internal, no lock).
    #[must_use]
    fn unmark_trimmed_block_inner(
        &mut self,
        block_number: u32,
        file_offset: u64,
        block_size: u32,
    ) -> bool {
        debug_assert!(block_number < self.data_block_count);
        debug_assert!(block_size.is_multiple_of(MB1 as u32));

        // If not marked, someone else already claimed it.
        if !self.trimmed_blocks.bitmap.check_bit(block_number as usize) {
            return false;
        }

        self.trimmed_blocks.bitmap.clear_bit(block_number as usize);
        self.trimmed_blocks.num_trimmed_blocks -= 1;

        let bit_base = (file_offset / MB1) as usize;
        let bit_count = block_size as usize / MB1 as usize;
        debug_assert!(
            self.anchored_space.bitmap.are_bits_set(bit_base, bit_count),
            "anchored space bits must be set for trimmed block {block_number} at offset {file_offset:#x}"
        );
        self.anchored_space.bitmap.clear_range(bit_base, bit_count);

        true
    }

    /// Release space to the free pool (internal, no lock).
    fn release_inner(&mut self, offset: u64, size: u32) {
        debug_assert!(offset.is_multiple_of(MB1));
        debug_assert!((size as u64).is_multiple_of(MB1));

        let bit_base = (offset / MB1) as usize;
        let bit_count = size as usize / MB1 as usize;

        if bit_base + bit_count > self.free_space.bitmap.len() {
            // Defensive: can't release beyond bitmap size.
            return;
        }

        debug_assert!(self.free_space.bitmap.are_bits_clear(bit_base, bit_count));
        self.free_space.bitmap.set_range(bit_base, bit_count);
        self.free_space.no_free_blocks = false;

        if (bit_base as u32) < self.free_space.lowest_bit_hint {
            self.free_space.lowest_bit_hint = bit_base as u32;
        }
    }

    /// Priority 1: free space pool allocation (internal, no lock).
    fn free_space_pool_alloc(&mut self, eof: &mut EofState, length: u32) -> Option<u64> {
        debug_assert!((length as u64).is_multiple_of(MB1));
        let bit_count = length as usize / MB1 as usize;

        // Fast-path skip for block-sized allocations.
        if length >= self.block_size && self.free_space.no_free_blocks {
            return None;
        }

        let result = self
            .free_space
            .bitmap
            .find_set_bits(bit_count, self.free_space.lowest_bit_hint as usize);

        match result {
            Some(bit_base) => {
                // Claim the space.
                self.free_space.bitmap.clear_range(bit_base, bit_count);
                self.free_space.lowest_bit_hint = (bit_base + bit_count) as u32;
                let max_offset = (bit_base + bit_count) as u64 * MB1;
                if eof.last_file_offset < max_offset {
                    eof.last_file_offset = max_offset;
                }
                Some(bit_base as u64 * MB1)
            }
            None => {
                if length <= self.block_size {
                    self.free_space.no_free_blocks = true;
                }
                None
            }
        }
    }

    /// Find and unanchor an in-memory-only soft-anchored block.
    fn find_and_unanchor_in_memory_inner(&mut self, bat: &Bat) -> Option<(u64, u32)> {
        if self.trimmed_blocks.num_trimmed_blocks == 0 {
            return None;
        }

        let block_size = self.block_size;

        // Try to find an in-memory-only soft-anchored block by scanning
        // the TrimmedBlock bitmap.
        let mut trimmed_found = 0u32;
        let total_trimmed = self.trimmed_blocks.num_trimmed_blocks;
        let mut hint = self.trimmed_blocks.lowest_block_number_hint as usize;

        while trimmed_found < total_trimmed {
            let block_number = match self.trimmed_blocks.bitmap.find_set_bits(1, hint) {
                Some(n) => n,
                None => break,
            };

            trimmed_found += 1;
            let mapping = bat.get_block_mapping(block_number as u32);

            // Block must be soft-anchored: unmapped/undefined state with non-zero file_megabyte.
            let state = mapping.bat_state();
            let is_unmapped = state == BatEntryState::Unmapped
                || state == BatEntryState::Undefined
                || state == BatEntryState::Zero
                || state == BatEntryState::NotPresent;

            debug_assert!(
                is_unmapped && mapping.file_megabyte() != 0,
                "trimmed block {block_number} is not soft-anchored"
            );

            // Check if it's in-memory only (not on-disk anchored).
            // Only blocks in TrimmedBlockTracker are considered here, and
            // those are only populated by flush() after WAL durability, so
            // the on-disk BAT already reflects the trim. Cross-block reclaim
            // is safe — the caller just needs to clear the old block's
            // file_megabyte and write its BAT page to cache.
            let file_offset = mapping.file_megabyte() as u64 * MB1;

            // Unmark the trimmed block.
            if self.unmark_trimmed_block_inner(block_number as u32, file_offset, block_size) {
                return Some((file_offset, block_number as u32));
            }

            hint = block_number + 1;
        }

        None
    }

    /// Compute excess block count (blocks that won't fit given current space).
    fn compute_excess_block_count(&self, eof: &EofState, max_offset: u64) -> u32 {
        // Count unallocated blocks.
        let total = self.data_block_count;
        // Available space: count of free bits in free space bitmap + anchored space
        // + space from zero_offset to file_length.
        let mut available_mb: u64 = 0;

        // Count free bits up to the bitmap.
        for i in 0..self.free_space.bitmap.len() {
            if self.free_space.bitmap.check_bit(i) {
                available_mb += 1;
            }
        }

        // Count anchored bits.
        for i in 0..self.anchored_space.bitmap.len() {
            if self.anchored_space.bitmap.check_bit(i) {
                available_mb += 1;
            }
        }

        // EOF space.
        let zero = eof.zero_offset.min(max_offset);
        if eof.file_length > zero {
            available_mb += (eof.file_length - zero) / MB1;
        }

        let block_mb = self.block_size as u64 / MB1;
        let available_blocks = available_mb / block_mb;
        let needed = total as u64;
        if needed > available_blocks {
            (needed - available_blocks) as u32
        } else {
            0
        }
    }
}

// ---------------------------------------------------------------------------
// Deferred space releases
// ---------------------------------------------------------------------------

/// Maximum number of deferred entries before trim forces a flush.
const DEFERRED_QUOTA: usize = 1024;

/// A space release that is deferred until its BAT change is durable on disk.
///
/// Without deferral, a crash could "teleport" data: a new block's data
/// appears at an old block's file offset because the old block's BAT
/// reverts to FullyPresent on replay.
struct DeferredRelease {
    file_offset: u64,
    size: u32,
    anchor: bool,
}

/// Entry in the deferred releases tracker, with generation stamp.
struct DeferredEntry {
    release: DeferredRelease,
    /// `None` = not yet committed to a WAL entry.
    /// `Some(gen)` = committed in flush generation `gen`.
    committed_gen: Option<u64>,
}

/// Tracks deferred space releases with generation-based promotion.
///
/// All state is behind a single `parking_lot::Mutex` — never held across
/// `.await`. The generation counter ensures entries are only promoted
/// after the flush that committed them reaches WAL durability.
pub(crate) struct DeferredReleases {
    inner: Mutex<DeferredInner>,
}

struct DeferredInner {
    entries: HashMap<u32, DeferredEntry>,
    /// Monotonically increasing. Bumped by each flush before commit.
    generation: u64,
}

impl DeferredReleases {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(DeferredInner {
                entries: HashMap::new(),
                generation: 0,
            }),
        }
    }

    /// Insert or replace a deferred release for a block.
    /// The entry starts uncommitted (no generation stamp).
    pub fn insert(&self, block_number: u32, file_offset: u64, size: u32, anchor: bool) {
        self.inner.lock().entries.insert(
            block_number,
            DeferredEntry {
                release: DeferredRelease {
                    file_offset,
                    size,
                    anchor,
                },
                committed_gen: None,
            },
        );
    }

    /// Remove a deferred entry for same-block reclaim.
    /// Returns the file offset if found. Same-block reclaim is always
    /// safe (OwnStale) regardless of committed state.
    pub fn remove(&self, block_number: u32) -> Option<u64> {
        self.inner
            .lock()
            .entries
            .remove(&block_number)
            .map(|e| e.release.file_offset)
    }

    /// Check whether a deferred entry exists for a block, and remove
    /// it if so. Returns true if removed.
    pub fn cancel(&self, block_number: u32) -> bool {
        self.inner.lock().entries.remove(&block_number).is_some()
    }

    /// Returns true if the number of deferred entries has reached the
    /// quota and a flush should be triggered to free them.
    pub fn needs_flush(&self) -> bool {
        self.inner.lock().entries.len() >= DEFERRED_QUOTA
    }

    /// Stamp all uncommitted entries with the current generation and
    /// bump the generation. Called at the start of flush(), before
    /// `commit()`. Returns the generation that was stamped.
    pub fn stamp_uncommitted(&self) -> u64 {
        let mut inner = self.inner.lock();
        inner.generation += 1;
        let flush_gen = inner.generation;
        for entry in inner.entries.values_mut() {
            if entry.committed_gen.is_none() {
                entry.committed_gen = Some(flush_gen);
            }
        }
        flush_gen
    }

    /// Drain all entries committed at or before the given generation.
    /// Returns (block_number, file_offset, size, anchor) tuples for
    /// promotion to the FreeSpaceTracker.
    pub fn drain_committed(&self, up_to_gen: u64) -> Vec<(u32, u64, u32, bool)> {
        let mut inner = self.inner.lock();
        let mut drained = Vec::new();
        inner.entries.retain(|&block, entry| {
            if entry.committed_gen.is_some_and(|g| g <= up_to_gen) {
                drained.push((
                    block,
                    entry.release.file_offset,
                    entry.release.size,
                    entry.release.anchor,
                ));
                false // remove from map
            } else {
                true // keep
            }
        });
        drained
    }
}

// ---------------------------------------------------------------------------
// VhdxFile — space allocation
// ---------------------------------------------------------------------------

use crate::AsyncFile;
use crate::bat::BlockMapping;
use crate::bat::BlockType;
use crate::open::VhdxFile;

impl<F: AsyncFile> VhdxFile<F> {
    /// Allocate space for a new block. Async — may extend the file.
    ///
    /// Called under `allocation_lock` (the `FreeSpaceWorkerLock` equivalent).
    /// The caller must pass `&mut EofState` obtained from locking
    /// `allocation_lock`.
    /// Tries pool → near-EOF → anchored, extends file and retries if needed.
    ///
    /// When `flags` includes [`AllocateFlags::ZERO`], the allocated region
    /// is guaranteed to be zeroed on disk before returning. Near-EOF
    /// allocations are inherently zero; pool/anchor allocations get an
    /// explicit zero-write.
    ///
    /// When `flags` includes [`AllocateFlags::ALIGNED`], the allocation is
    /// aligned to `block_alignment`.
    pub(crate) async fn allocate_space(
        &self,
        eof: &mut EofState,
        size: u32,
        flags: AllocateFlags,
    ) -> Result<AllocateResult, VhdxIoError> {
        debug_assert!(
            (size as u64).is_multiple_of(MB1),
            "allocation size must be MB1-aligned"
        );

        loop {
            // Try priorities 1–3 (pool, near-EOF, anchored).
            let result =
                self.free_space
                    .try_allocate_with_bat(eof, size, flags.aligned(), &self.bat);

            if let Some(alloc) = result {
                // If this was a cross-block soft-anchor reclaim, clear the
                // old block's file_megabyte in BatState and write its BAT
                // page to cache. The old block's trim is already durable
                // (TrimmedBlockTracker is only populated after flush), so
                // no extra flush is needed — just BAT write ordering.
                if let Some(old_block) = alloc.unanchored_block {
                    let old_mapping = self.bat.get_block_mapping(old_block);
                    let cleared_mapping = BlockMapping::new()
                        .with_bat_state(old_mapping.bat_state())
                        .with_transitioning_to_fully_present(false)
                        .with_file_megabyte(0);
                    self.bat.set_block_mapping(old_block, cleared_mapping);

                    // Write old block's BAT page to cache (async).
                    // LOCK AUDIT: allocation_lock held.
                    self.bat
                        .write_block_mapping(
                            &self.cache,
                            BlockType::Payload,
                            old_block,
                            cleared_mapping,
                            None,
                        )
                        .await?;
                }

                if flags.zero() && !alloc.state.is_zero() {
                    // Space from pool/anchor may contain stale data — zero it.
                    self.file
                        .zero_range(alloc.file_offset, size as u64)
                        .await
                        .map_err(|e| VhdxIoErrorInner::ZeroBlock {
                            err: e,
                            file_offset: alloc.file_offset,
                        })?;
                }
                return Ok(alloc);
            }

            // Priority 4: extend EOF.
            let block_alignment = self.free_space.block_alignment();
            let target = eof.required_file_length(block_alignment, size, flags.aligned());
            // LOCK AUDIT: bat_state read-lock dropped (end of block above). allocation_lock held (async Mutex — OK across .await).
            self.file
                .set_file_size(target)
                .await
                .map_err(|e| VhdxIoErrorInner::ExtendFile {
                    err: e,
                    target_file_size: target,
                })?;
            eof.complete_file_extend(&self.free_space, target);
            // Retry — will succeed from near-EOF space.
        }
    }

    /// Truncate the file to reclaim unused trailing space.
    ///
    /// Shrinks the file to just past the highest in-use offset, rounded
    /// up to MB1. For fully-allocated (fixed) disks, reserves extra
    /// space for blocks that haven't been allocated yet.
    ///
    /// Called during [`close()`](Self::close) after all WAL entries are
    /// drained. Must NOT be called while the log task is running.
    pub(crate) async fn truncate_file(&self) -> Result<(), VhdxIoError> {
        let mut eof = self.allocation_lock.lock().await;
        let target = self
            .free_space
            .truncate_target(&eof, self.is_fully_allocated());

        // Only shrink, never grow. And don't bother if the savings
        // are less than the EOF extension length (avoids thrashing
        // on files that are close to their minimum size).
        if target < eof.file_length && eof.file_length - target >= eof.eof_extension_length as u64 {
            // Round up to MB1.
            let target_aligned = (target + MB1 - 1) & !(MB1 - 1);
            self.file.set_file_size(target_aligned).await.map_err(|e| {
                VhdxIoErrorInner::TruncateFile {
                    err: e,
                    target_file_size: target_aligned,
                }
            })?;
            self.free_space.apply_truncate(&mut eof, target_aligned);
        }

        Ok(())
    }

    /// Compute the cache [`PageKey`] for the BAT page containing the given
    /// payload block's entry.
    ///
    /// Used by crash-consistency tests to inspect `pre_log_fsn` on BAT pages.
    #[cfg(test)]
    pub(crate) fn bat_page_key_for_block(&self, block_number: u32) -> crate::cache::PageKey {
        use crate::bat::BAT_TAG;
        use crate::format::CACHE_PAGE_SIZE;

        let entry_index = self.bat.payload_entry_index(block_number);
        let page_offset = (entry_index as u64 * 8) & !(CACHE_PAGE_SIZE - 1);
        crate::cache::PageKey {
            tag: BAT_TAG,
            offset: page_offset,
        }
    }
}

// ---------------------------------------------------------------------------
// Test-only helpers on FreeSpaceTracker
// ---------------------------------------------------------------------------

#[cfg(test)]
impl FreeSpaceTracker {
    /// Try to allocate space using priorities 1–3 (pool, near-EOF, anchored)
    /// without a BAT state (skips priority 3).
    pub fn try_allocate(
        &self,
        eof: &mut EofState,
        size: u32,
        aligned: bool,
    ) -> Option<AllocateResult> {
        self.try_allocate_inner(eof, size, aligned, None)
    }

    /// Find and unanchor a soft-anchored block (in-memory only anchors).
    pub fn find_and_unanchor_in_memory(&self, bat: &Bat) -> Option<(u64, u32)> {
        let mut inner = self.inner.lock();
        inner.find_and_unanchor_in_memory_inner(bat)
    }

    /// Check if a range is in use (for debug/validation).
    pub fn is_range_in_use(&self, eof: &EofState, offset: u64, length: u32) -> bool {
        let inner = self.inner.lock();
        debug_assert!(offset.is_multiple_of(MB1));
        debug_assert!((length as u64).is_multiple_of(MB1));

        if eof.file_length < offset || eof.file_length - offset < length as u64 {
            return true;
        }

        let bit_base = (offset / MB1) as usize;
        let bit_count = length as usize / MB1 as usize;
        !inner.free_space.bitmap.are_bits_set(bit_base, bit_count)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bat::BlockMapping;
    use crate::format::BatEntryState;

    // -- Bitmap unit tests --

    #[test]
    fn bitmap_set_clear_range() {
        let mut bm = SpaceBitmap::new(128);
        assert!(bm.are_bits_clear(0, 128));

        bm.set_range(10, 20);
        assert!(bm.are_bits_set(10, 20));
        assert!(bm.are_bits_clear(0, 10));
        assert!(bm.are_bits_clear(30, 98));

        bm.clear_range(15, 5);
        assert!(bm.are_bits_set(10, 5));
        assert!(bm.are_bits_clear(15, 5));
        assert!(bm.are_bits_set(20, 10));
    }

    #[test]
    fn bitmap_find_set_bits() {
        let mut bm = SpaceBitmap::new(64);
        // Create a run of 8 set bits starting at index 20.
        bm.set_range(20, 8);

        assert_eq!(bm.find_set_bits(8, 0), Some(20));
        assert_eq!(bm.find_set_bits(8, 20), Some(20));
        assert_eq!(bm.find_set_bits(9, 0), None);
        assert_eq!(bm.find_set_bits(1, 25), Some(25));
    }

    #[test]
    fn bitmap_find_set_bits_wraps_hint() {
        let mut bm = SpaceBitmap::new(64);
        // Run at the beginning.
        bm.set_range(0, 4);

        // Hint past the run — should wrap and find it.
        assert_eq!(bm.find_set_bits(4, 50), Some(0));
    }

    /// Regression: find_set_bits must find a valid non-wrapping run at
    /// the bitmap start even when the scan first encounters a wrapping
    /// candidate that spans the bitmap end→start boundary.
    ///
    /// Bitmap (8 bits): [1,1,1,1,0,0,1,1]
    ///                   ^-------^         valid run of 4 at index 0
    ///                               ^--^  bits 6-7 set
    ///
    /// With hint=5, the scan visits: 5(0),6(1),7(1),0(1),1(1) — a run
    /// of 4 starting at index 6, but it wraps (6+4=10>8). After
    /// rejecting the wrap, bits 0-3 must still be found as a valid run.
    #[test]
    fn bitmap_find_set_bits_rejected_wrap_finds_later_run() {
        let mut bm = SpaceBitmap::new(8);
        bm.set_range(0, 4); // bits 0,1,2,3
        bm.set_range(6, 2); // bits 6,7

        // Hint=5: scan starts at 5, wraps, should find run at 0.
        assert_eq!(
            bm.find_set_bits(4, 5),
            Some(0),
            "should find non-wrapping run [0..4) after rejecting wrap at 6"
        );
    }

    #[test]
    fn bitmap_are_bits_set_clear() {
        let mut bm = SpaceBitmap::new(32);
        bm.set_all();
        assert!(bm.are_bits_set(0, 32));
        assert!(!bm.are_bits_clear(0, 32));

        bm.clear_bit(16);
        assert!(!bm.are_bits_set(0, 32));
        assert!(!bm.are_bits_set(16, 1));
        assert!(bm.are_bits_clear(16, 1));
    }

    #[test]
    fn bitmap_empty_and_full() {
        let bm_empty = SpaceBitmap::new(0);
        assert_eq!(bm_empty.len(), 0);
        assert_eq!(bm_empty.find_set_bits(1, 0), None);

        let mut bm = SpaceBitmap::new(1);
        assert!(bm.are_bits_clear(0, 1));
        bm.set_bit(0);
        assert!(bm.are_bits_set(0, 1));
    }

    // -- FreeSpaceTracker initialization tests --

    /// Helper: create a tracker for a small test file.
    fn make_test_tracker(file_mb: u64, block_size_mb: u32) -> (FreeSpaceTracker, EofState) {
        make_test_tracker_aligned(file_mb, block_size_mb, 0)
    }

    fn make_test_tracker_aligned(
        file_mb: u64,
        block_size_mb: u32,
        block_alignment: u32,
    ) -> (FreeSpaceTracker, EofState) {
        let file_length = file_mb * MB1;
        let block_size = block_size_mb * MB1 as u32;
        let data_block_count = 16; // arbitrary for testing

        FreeSpaceTracker::new(
            file_length,
            block_size,
            block_alignment,
            MB1,        // header_area_size = 1 MB
            MB1,        // log_offset = 1 MB
            MB1 as u32, // log_length = 1 MB
            2 * MB1,    // bat_offset = 2 MB
            MB1 as u32, // bat_length = 1 MB
            3 * MB1,    // metadata_offset = 3 MB
            MB1 as u32, // metadata_length = 1 MB
            data_block_count,
        )
        .unwrap()
    }

    #[test]
    fn init_marks_header_in_use() {
        let (tracker, eof) = make_test_tracker(10, 2);
        // Header area (0..1MB) should be in-use.
        assert!(tracker.is_range_in_use(&eof, 0, MB1 as u32));
    }

    #[test]
    fn init_marks_regions_in_use() {
        let (tracker, eof) = make_test_tracker(10, 2);
        // Log (1..2MB), BAT (2..3MB), metadata (3..4MB) should be in-use.
        assert!(tracker.is_range_in_use(&eof, MB1, MB1 as u32));
        assert!(tracker.is_range_in_use(&eof, 2 * MB1, MB1 as u32));
        assert!(tracker.is_range_in_use(&eof, 3 * MB1, MB1 as u32));
    }

    #[test]
    fn overlap_detection() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        // Try to mark the header area again — should fail with RangeCollision.
        let result = tracker.mark_range_in_use(&mut eof, 0, MB1 as u32);
        assert!(matches!(result, Err(CorruptionType::RangeCollision)));
    }

    #[test]
    fn range_beyond_eof_detected() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        // Try to mark a range that extends beyond file length.
        let result = tracker.mark_range_in_use(&mut eof, 9 * MB1, 2 * MB1 as u32);
        assert!(matches!(result, Err(CorruptionType::RangeBeyondEof)));
    }

    // -- Allocation priority tests --

    #[test]
    fn allocate_from_free_pool() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        // Mark offset 4MB in-use (simulating BAT parse finding a block there).
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);
        // Now zero_offset = 5*MB. Near-EOF = 5..10 MB (5 MB).
        // Bit 4 is in-use (cleared). Release it back to pool.
        tracker.release(4 * MB1, MB1 as u32);

        // Priority 1: should find the released space.
        let result = tracker.try_allocate(&mut eof, MB1 as u32, false);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.file_offset, 4 * MB1);
        assert!(!r.state.is_safe());
    }

    #[test]
    fn allocate_from_eof_space() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        tracker.complete_initialization(&eof);

        // After initialization, zero_offset = 4*MB, file_length = 10*MB.
        // Near-EOF space = 6 MB.
        let result = tracker.try_allocate(&mut eof, 2 * MB1 as u32, false);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.file_offset, 4 * MB1);
        assert!(r.state.is_safe()); // Beyond old zero_offset.
    }

    #[test]
    fn allocate_extends_eof() {
        // Create a tracker with only 4MB (all in-use by regions).
        let (tracker, mut eof) = make_test_tracker(4, 2);
        tracker.complete_initialization(&eof);

        // No free space, no near-EOF space.
        let result = tracker.try_allocate(&mut eof, MB1 as u32, false);
        assert!(result.is_none());

        // Compute required length and extend.
        let target = eof.required_file_length(tracker.block_alignment(), MB1 as u32, false);
        assert!(target > 4 * MB1);

        eof.complete_file_extend(&tracker, target);

        // Now retry — should succeed from near-EOF.
        let result = tracker.try_allocate(&mut eof, MB1 as u32, false);
        assert!(result.is_some());
        assert!(result.unwrap().state.is_safe());
    }

    #[test]
    fn allocate_alignment() {
        // 20MB file, 4MB block size, 4MB alignment.
        let (tracker, mut eof) = make_test_tracker_aligned(20, 4, 4 * MB1 as u32);
        tracker.complete_initialization(&eof);

        // zero_offset = 4MB (after regions).
        // Aligned allocation from EOF: should be at 4MB (already aligned).
        let result = tracker.try_allocate(&mut eof, 4 * MB1 as u32, true);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.file_offset % (4 * MB1), 0);
    }

    #[test]
    fn allocate_sets_no_free_blocks_flag() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        tracker.complete_initialization(&eof);

        // Exhaust near-EOF space with pool allocations — first exhaust pool.
        // After init, pool is empty (regions fill 0..4MB, rest is EOF space).
        // Try pool-only: allocate 1MB from pool (should fail, and set flag).
        // But near-EOF will succeed before we get to that.
        //
        // Instead, fill up all space and verify the flag works.
        // Allocate all 6 MB of EOF space.
        for _ in 0..6 {
            tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        }
        // Now no space left.
        let result = tracker.try_allocate(&mut eof, MB1 as u32, false);
        assert!(result.is_none());
    }

    // -- Soft anchoring tests --

    /// Create a minimal `Bat` for soft-anchor tests with one anchored block.
    ///
    /// Uses 2 MiB block size, 512-byte sectors, no parent. The
    /// `data_block_count` parameter controls how many payload entries
    /// the BAT has.
    fn make_test_bat_with_anchored_block(
        block_number: u32,
        file_megabyte: u32,
        data_block_count: u32,
    ) -> Bat {
        let block_size = 2 * MB1 as u32;
        let disk_size = data_block_count as u64 * block_size as u64;
        let mut bat = Bat::new(disk_size, block_size, 512, false, MB1 as u32).unwrap();
        bat.init_test_payload_mappings();
        bat.set_block_mapping(
            block_number,
            BlockMapping::new()
                .with_bat_state(BatEntryState::Unmapped)
                .with_file_megabyte(file_megabyte),
        );
        bat
    }

    #[test]
    fn mark_and_find_anchored_block() {
        let (tracker, _eof) = make_test_tracker(20, 2);
        // Mark block 3 as trimmed at file offset 6*MB.
        tracker
            .mark_trimmed_block(3, 6 * MB1, 2 * MB1 as u32)
            .unwrap();

        // Verify anchored space bits are set.
        let inner = tracker.inner.lock();
        assert!(inner.anchored_space.bitmap.are_bits_set(6, 2));
        assert!(inner.trimmed_blocks.bitmap.check_bit(3));
        assert_eq!(inner.trimmed_blocks.num_trimmed_blocks, 1);
    }

    #[test]
    fn unmark_trimmed_block() {
        let (tracker, _eof) = make_test_tracker(20, 2);
        tracker
            .mark_trimmed_block(3, 6 * MB1, 2 * MB1 as u32)
            .unwrap();
        assert!(tracker.unmark_trimmed_block(3, 6 * MB1, 2 * MB1 as u32));

        let inner = tracker.inner.lock();
        assert!(inner.anchored_space.bitmap.are_bits_clear(6, 2));
        assert!(!inner.trimmed_blocks.bitmap.check_bit(3));
        assert_eq!(inner.trimmed_blocks.num_trimmed_blocks, 0);
    }

    #[test]
    fn find_and_unanchor_in_memory() {
        let (tracker, _eof) = make_test_tracker(20, 2);
        // Mark block 5 as trimmed at file offset 8*MB.
        tracker
            .mark_trimmed_block(5, 8 * MB1, 2 * MB1 as u32)
            .unwrap();

        let bat = make_test_bat_with_anchored_block(5, 8, 16);

        let result = tracker.find_and_unanchor_in_memory(&bat);
        assert!(result.is_some());
        let (offset, block_num) = result.unwrap();
        assert_eq!(offset, 8 * MB1);
        assert_eq!(block_num, 5);

        // After unanchoring, the trimmed block should be unmarked.
        let inner = tracker.inner.lock();
        assert!(!inner.trimmed_blocks.bitmap.check_bit(5));
        assert_eq!(inner.trimmed_blocks.num_trimmed_blocks, 0);
    }

    #[test]
    fn anchored_space_before_eof_extend() {
        // Set up a full file with no free pool and no EOF space,
        // but with a soft-anchored block.
        let (tracker, mut eof) = make_test_tracker(10, 2);

        // Mark block 2 as trimmed at offset 6*MB.
        tracker
            .mark_trimmed_block(2, 6 * MB1, 2 * MB1 as u32)
            .unwrap();
        // Mark remaining free space as in-use so pool is empty.
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, MB1 as u32)
            .unwrap();
        tracker
            .mark_range_in_use(&mut eof, 5 * MB1, MB1 as u32)
            .unwrap();
        tracker
            .mark_range_in_use(&mut eof, 8 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);

        let bat = make_test_bat_with_anchored_block(2, 6, 16);

        // Should find anchored space (priority 3) instead of extending EOF.
        let result = tracker.try_allocate_with_bat(&mut eof, 2 * MB1 as u32, false, &bat);
        assert!(result.is_some());
        let r = result.unwrap();
        assert_eq!(r.file_offset, 6 * MB1);
    }

    // -- Release tests --

    #[test]
    fn release_then_reallocate() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        tracker.complete_initialization(&eof);

        // Allocate from EOF space.
        let r1 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        let offset = r1.file_offset;

        // Release it back to free pool.
        tracker.release(offset, MB1 as u32);

        // Allocate again — should reuse the released space.
        let r2 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r2.file_offset, offset);
    }

    // -- Truncation test --

    #[test]
    fn truncate_shrinks_bitmaps() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        tracker.complete_initialization(&eof);

        assert_eq!(eof.file_length, 10 * MB1);

        tracker.apply_truncate(&mut eof, 6 * MB1);
        assert_eq!(eof.file_length, 6 * MB1);
    }

    // -- Bitmap resize test --

    #[test]
    fn bitmap_resize_preserves_data() {
        let mut bm = SpaceBitmap::new(32);
        bm.set_range(10, 10);

        bm.resize(64);
        assert_eq!(bm.len(), 64);
        assert!(bm.are_bits_set(10, 10));
        assert!(bm.are_bits_clear(20, 44));

        bm.resize(16);
        assert_eq!(bm.len(), 16);
        assert!(bm.are_bits_set(10, 6)); // only 10..16 remains
    }

    // -- Priority cascade test --

    #[test]
    fn priority_cascade_pool_then_eof_then_anchor_then_extend() {
        // Walk through all 4 priorities in sequence.
        let (tracker, mut eof) = make_test_tracker(10, 2);

        // Mark 4..5 MB in-use (a data block during BAT parse).
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, MB1 as u32)
            .unwrap();
        // Mark 5..7 MB in-use, then mark as soft-anchored (trimmed block 1).
        // Always mark in-use first, then mark as trimmed.
        tracker
            .mark_range_in_use(&mut eof, 5 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker
            .mark_trimmed_block(1, 5 * MB1, 2 * MB1 as u32)
            .unwrap();
        // Mark 7..8 MB in-use.
        tracker
            .mark_range_in_use(&mut eof, 7 * MB1, MB1 as u32)
            .unwrap();

        tracker.complete_initialization(&eof);
        // zero_offset = 8 MB, file_length = 10 MB.
        // Pool: empty (all bits 0..8 are cleared). Near-EOF: 8..10 (2 MB).

        // Release bit 4 back to pool.
        tracker.release(4 * MB1, MB1 as u32);

        // Create BAT for soft-anchor lookup.
        let bat = make_test_bat_with_anchored_block(1, 5, 16);

        // Priority 1: pool (offset 4 MB).
        let r1 = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r1.file_offset, 4 * MB1);
        assert!(!r1.state.is_safe());

        // Pool now empty. Priority 2: near-EOF (offset 8 MB).
        let r2 = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r2.file_offset, 8 * MB1);
        assert!(r2.state.is_safe());

        // Take the second EOF MB too.
        let r3 = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r3.file_offset, 9 * MB1);
        assert!(r3.state.is_safe());

        // Pool and EOF exhausted. Priority 3: soft-anchored (offset 5 MB).
        // The block is 2 MB but we only need 1 MB — excess goes to pool.
        let r4 = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r4.file_offset, 5 * MB1);
        assert!(!r4.state.is_safe());

        // The excess 1 MB from the anchored block should now be in pool.
        let r5 = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r5.file_offset, 6 * MB1);
        assert!(!r5.state.is_safe());

        // Everything exhausted. Priority 4: returns None.
        let r6 = tracker.try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat);
        assert!(r6.is_none());

        // Extend EOF, then retry.
        let target = eof.required_file_length(tracker.block_alignment(), MB1 as u32, false);
        eof.complete_file_extend(&tracker, target);
        let r7 = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert!(r7.state.is_safe());
        assert_eq!(r7.file_offset, 10 * MB1);
    }

    // -- Aligned allocation from pool test --

    #[test]
    fn aligned_alloc_from_pool() {
        // 20 MB file, 4 MB block size, 4 MB alignment.
        let (tracker, mut eof) = make_test_tracker_aligned(20, 4, 4 * MB1 as u32);

        // Mark 4..8 MB in-use, then release to create a 4MB pool hole at an aligned offset.
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, 4 * MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);
        tracker.release(4 * MB1, 4 * MB1 as u32);

        // Pool allocation ignores alignment (alignment only applies to near-EOF).
        let result = tracker
            .try_allocate(&mut eof, 4 * MB1 as u32, true)
            .unwrap();
        assert_eq!(result.file_offset, 4 * MB1);
        assert!(!result.state.is_safe());
    }

    // -- Unaligned EOF skip test --

    #[test]
    fn aligned_alloc_skips_unaligned_eof_offset() {
        // 20 MB file, 4 MB block size, 4 MB alignment.
        let (tracker, mut eof) = make_test_tracker_aligned(20, 4, 4 * MB1 as u32);

        // Mark 4..5 MB in-use. This pushes zero_offset to 5 MB (not 4MB-aligned).
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);
        // zero_offset = 5 MB. Aligned to 4 MB → round up to 8 MB.
        // So the allocation should come from offset 8 MB (skipping 5..8).

        let result = tracker
            .try_allocate(&mut eof, 4 * MB1 as u32, true)
            .unwrap();
        assert_eq!(result.file_offset, 8 * MB1);
        assert!(result.state.is_safe());
    }

    // -- Bitmap resize on file extend --

    #[test]
    fn complete_file_extend_grows_bitmaps() {
        let (tracker, mut eof) = make_test_tracker(4, 2);
        tracker.complete_initialization(&eof);

        // Bitmap should be 4 bits (4 MB / 1 MB).
        {
            let inner = tracker.inner.lock();
            assert!(inner.free_space.bitmap.len() >= 4);
        }

        // Extend to 100 MB.
        eof.complete_file_extend(&tracker, 100 * MB1);
        assert_eq!(eof.file_length, 100 * MB1);

        {
            let inner = tracker.inner.lock();
            // Bitmap must have grown to at least 100 bits.
            assert!(inner.free_space.bitmap.len() >= 100);
            assert!(inner.anchored_space.bitmap.len() >= 100);
        }

        // Near-EOF space should now be available.
        let result = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert!(result.state.is_safe());
    }

    // -- no_free_blocks flag reset on release --

    #[test]
    fn no_free_blocks_flag_resets_on_release() {
        let (tracker, mut eof) = make_test_tracker(6, 2);
        tracker.complete_initialization(&eof);

        // Exhaust all space: 2 MB of near-EOF (6-4=2).
        tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert!(tracker.try_allocate(&mut eof, MB1 as u32, false).is_none());

        // The no_free_blocks flag should be set now.
        {
            let inner = tracker.inner.lock();
            assert!(inner.free_space.no_free_blocks);
        }

        // Release 1 MB back.
        tracker.release(4 * MB1, MB1 as u32);

        // Flag should be cleared.
        {
            let inner = tracker.inner.lock();
            assert!(!inner.free_space.no_free_blocks);
        }

        // Should be able to allocate again.
        let result = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(result.file_offset, 4 * MB1);
        assert!(!result.state.is_safe());
    }

    // -- Fragmented pool test --

    #[test]
    fn fragmented_pool_allocates_from_lowest_hint() {
        let (tracker, mut eof) = make_test_tracker(20, 2);
        // Mark a contiguous range 4..11 MB in-use during BAT parse.
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, 7 * MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);
        // zero_offset = 11 MB. Near-EOF = 11..20 (9 MB).
        // Pool: empty (bits 0..11 all cleared).

        // Release scattered 1MB blocks to create fragmentation.
        tracker.release(10 * MB1, MB1 as u32);
        tracker.release(8 * MB1, MB1 as u32);
        tracker.release(6 * MB1, MB1 as u32);
        tracker.release(4 * MB1, MB1 as u32);

        // Pool should find the lowest free bit first.
        let r1 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r1.file_offset, 4 * MB1);

        let r2 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r2.file_offset, 6 * MB1);

        let r3 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r3.file_offset, 8 * MB1);

        let r4 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r4.file_offset, 10 * MB1);

        // Pool exhausted — next allocation comes from near-EOF.
        let r5 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r5.file_offset, 11 * MB1);
        assert!(r5.state.is_safe());
    }

    // -- Multi-MB allocation from pool --

    #[test]
    fn pool_allocates_contiguous_multi_mb() {
        let (tracker, mut eof) = make_test_tracker(20, 2);
        // Mark a contiguous 4 MB region (bits 4..8) in-use, then release.
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, 4 * MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);
        tracker.release(4 * MB1, 4 * MB1 as u32);

        // Now request a 3 MB allocation from pool — should find the 4MB hole.
        let result = tracker
            .try_allocate(&mut eof, 3 * MB1 as u32, false)
            .unwrap();
        assert_eq!(result.file_offset, 4 * MB1);
        assert!(!result.state.is_safe());

        // 1 MB of the hole (bit 7) is still in pool.
        let r2 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r2.file_offset, 7 * MB1);
    }

    // -- Truncation clamps zero_offset --

    #[test]
    fn truncate_clamps_zero_offset() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        tracker.complete_initialization(&eof);
        // zero_offset = 4 MB, file_length = 10 MB.

        // Allocate some EOF space to advance zero_offset.
        tracker
            .try_allocate(&mut eof, 3 * MB1 as u32, false)
            .unwrap();
        assert_eq!(eof.zero_offset, 7 * MB1);

        // Truncate file to 5 MB.
        tracker.apply_truncate(&mut eof, 5 * MB1);
        assert_eq!(eof.file_length, 5 * MB1);
        // zero_offset should be clamped to file_length.
        assert!(eof.zero_offset <= 5 * MB1);
    }

    // -- Multiple anchored blocks: only one reclaimed per allocate --

    #[test]
    fn multiple_anchored_blocks_reclaimed_one_at_a_time() {
        let (tracker, mut eof) = make_test_tracker(20, 2);
        // Mark anchored regions in-use first (standard sequence),
        // then mark as trimmed.
        tracker
            .mark_range_in_use(&mut eof, 6 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker
            .mark_trimmed_block(2, 6 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker
            .mark_range_in_use(&mut eof, 10 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker
            .mark_trimmed_block(5, 10 * MB1, 2 * MB1 as u32)
            .unwrap();

        // Fill all remaining space so pool + EOF are empty.
        tracker
            .mark_range_in_use(&mut eof, 4 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker
            .mark_range_in_use(&mut eof, 8 * MB1, 2 * MB1 as u32)
            .unwrap();
        tracker
            .mark_range_in_use(&mut eof, 12 * MB1, 8 * MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);

        // BAT with both blocks anchored.
        let bat = make_test_bat_with_anchored_block(2, 6, 16);
        bat.set_block_mapping(
            5,
            BlockMapping::new()
                .with_bat_state(BatEntryState::Unmapped)
                .with_file_megabyte(10),
        );

        // First allocate gets block 2 (lowest block number).
        let r1 = tracker
            .try_allocate_with_bat(&mut eof, 2 * MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r1.file_offset, 6 * MB1);
        assert!(!r1.state.is_safe());

        // Second allocate gets block 5.
        let r2 = tracker
            .try_allocate_with_bat(&mut eof, 2 * MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r2.file_offset, 10 * MB1);
        assert!(!r2.state.is_safe());

        // No more anchored blocks.
        assert!(
            tracker
                .try_allocate_with_bat(&mut eof, 2 * MB1 as u32, false, &bat)
                .is_none()
        );
    }

    // -- Anchored block larger than requested: excess goes to pool --

    #[test]
    fn anchored_block_excess_released_to_pool() {
        let (tracker, mut eof) = make_test_tracker(10, 2); // block_size = 2 MB
        // Anchor block 0 at offset 4..6 MB.
        tracker
            .mark_trimmed_block(0, 4 * MB1, 2 * MB1 as u32)
            .unwrap();
        // Fill the rest.
        tracker
            .mark_range_in_use(&mut eof, 6 * MB1, 4 * MB1 as u32)
            .unwrap();
        tracker.complete_initialization(&eof);

        let bat = make_test_bat_with_anchored_block(0, 4, 16);

        // Request only 1 MB from a 2 MB anchored block.
        let r = tracker
            .try_allocate_with_bat(&mut eof, MB1 as u32, false, &bat)
            .unwrap();
        assert_eq!(r.file_offset, 4 * MB1);

        // The excess 1 MB (at offset 5 MB) should now be in the free pool.
        let r2 = tracker.try_allocate(&mut eof, MB1 as u32, false).unwrap();
        assert_eq!(r2.file_offset, 5 * MB1);
        assert!(!r2.state.is_safe());
    }

    // -- required_file_length respects alignment --

    #[test]
    fn required_file_length_with_alignment() {
        let (tracker, eof) = make_test_tracker_aligned(4, 4, 4 * MB1 as u32);
        tracker.complete_initialization(&eof);
        // zero_offset = 4 MB (already aligned).

        let target = eof.required_file_length(tracker.block_alignment(), 4 * MB1 as u32, true);
        // Should be at least file_length + extension_length.
        assert!(target >= 4 * MB1 + DEFAULT_EOF_EXTENSION_LENGTH as u64);
        // And aligned target should fit the request.
        assert!(target >= 4 * MB1 + 4 * MB1);
    }

    // -- Zero-length mark is a no-op --

    #[test]
    fn mark_zero_length_is_noop() {
        let (tracker, mut eof) = make_test_tracker(10, 2);
        assert!(tracker.mark_range_in_use(&mut eof, 4 * MB1, 0).is_ok());
        // The range should still be free.
        assert!(!tracker.is_range_in_use(&eof, 4 * MB1, MB1 as u32));
    }
}
