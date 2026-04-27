// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX block trim (unmap) support.
//!
//! Implements the `VhdxFile::trim()` method that transitions blocks to
//! unmapped states, releasing file space back to the free pool or
//! soft-anchoring it for later reuse.

use crate::AsyncFile;
use crate::bat::BlockMapping;
use crate::bat::BlockType;
use crate::error::VhdxIoError;
use crate::error::VhdxIoErrorInner;
use crate::format::BatEntryState;
use crate::format::MB1;
use crate::header::WriteMode;
use crate::open::VhdxFile;

/// Trim mode determining the target block state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimMode {
    /// Move blocks to the Unmapped (trimmed) state, keeping file offset
    /// as a soft anchor for potential reuse.
    ///
    /// Denied for Undefined blocks: a block that was never written
    /// should stay Undefined (preserves backup semantics — backup tools
    /// skip Undefined blocks, but not Unmapped ones).
    FileSpace,

    /// Move blocks to the Undefined state. Soft anchor may be kept or
    /// cleared depending on the original state.
    FreeSpace,

    /// Move blocks to the Zero state, clearing the file offset.
    Zero,

    /// Move blocks to the NotPresent (transparent) state, clearing the
    /// file offset. For differencing disks, reads fall through to parent.
    ///
    /// Allowed on fully-allocated (fixed) disks.
    MakeTransparent,

    /// Remove soft anchors from trimmed/undefined blocks without changing
    /// their state. Clears file_megabyte if the block is soft-anchored.
    ///
    /// Allowed on fully-allocated (fixed) disks. Does not change data
    /// content, so DataWriteGuid is not updated.
    RemoveSoftAnchors,
}

/// Builder for a trim operation on a VHDX file.
///
/// Created via [`VhdxFile::trim`]. Required parameters (`mode`, `offset`,
/// `length`) are provided at construction; optional flags default to the
/// safe/common values and can be overridden with builder methods.
#[derive(Debug, Clone)]
pub struct TrimRequest {
    mode: TrimMode,
    offset: u64,
    length: u64,
    skip_disk_size_check: bool,
    skip_write_guid_change: bool,
}

impl TrimRequest {
    /// Create a new trim request.
    ///
    /// * `mode` - Determines the target block state.
    /// * `offset` - Virtual disk byte offset (must be sector-aligned).
    /// * `length` - Length in bytes (must be sector-aligned).
    pub fn new(mode: TrimMode, offset: u64, length: u64) -> Self {
        Self {
            mode,
            offset,
            length,
            skip_disk_size_check: false,
            skip_write_guid_change: false,
        }
    }

    /// Skip bounds checking against the virtual disk size.
    pub fn skip_disk_size_check(mut self, skip: bool) -> Self {
        self.skip_disk_size_check = skip;
        self
    }

    /// Don't update DataWriteGuid when trimming.
    pub fn skip_write_guid_change(mut self, skip: bool) -> Self {
        self.skip_write_guid_change = skip;
        self
    }
}

/// Returns true if the given trim mode is allowed on fully-allocated (fixed) disks.
fn mode_allowed_on_fixed(mode: TrimMode) -> bool {
    matches!(
        mode,
        TrimMode::MakeTransparent | TrimMode::RemoveSoftAnchors
    )
}

/// Returns true if this trim mode should skip the DataWriteGuid update.
fn mode_skips_write_guid(mode: TrimMode) -> bool {
    matches!(mode, TrimMode::RemoveSoftAnchors)
}

/// Convert a block mapping according to the trim mode.
///
/// Returns the new mapping, which may be identical to `old` (no-op).
fn convert_mapping(mode: TrimMode, old: BlockMapping) -> BlockMapping {
    let state = old.bat_state();
    match mode {
        TrimMode::FileSpace => convert_file_space(state, old),
        TrimMode::FreeSpace => convert_free_space(state, old),
        TrimMode::Zero => convert_zero(state, old),
        TrimMode::MakeTransparent => convert_make_transparent(state, old),
        TrimMode::RemoveSoftAnchors => convert_remove_soft_anchors(old),
    }
}

/// FileSpace: FullyPresent/PartiallyPresent → Unmapped (keep soft anchor).
/// All other states are no-ops.
fn convert_file_space(state: BatEntryState, old: BlockMapping) -> BlockMapping {
    match state {
        BatEntryState::FullyPresent | BatEntryState::PartiallyPresent => {
            BlockMapping::new()
                .with_bat_state(BatEntryState::Unmapped)
                .with_transitioning_to_fully_present(false)
                .with_file_megabyte(old.file_megabyte()) // keep as soft anchor
        }
        _ => old, // NotPresent, Undefined, Zero, Unmapped → no change
    }
}

/// FreeSpace: FullyPresent/PartiallyPresent → Undefined (clear offset, release space).
/// Zero → Undefined (clear offset).
/// Unmapped → Undefined (keep soft anchor).
/// Others → no change.
fn convert_free_space(state: BatEntryState, old: BlockMapping) -> BlockMapping {
    match state {
        BatEntryState::FullyPresent | BatEntryState::PartiallyPresent => {
            // Release space — clear file offset.
            BlockMapping::new()
                .with_bat_state(BatEntryState::Undefined)
                .with_transitioning_to_fully_present(false)
                .with_file_megabyte(0)
        }
        BatEntryState::Zero => BlockMapping::new()
            .with_bat_state(BatEntryState::Undefined)
            .with_transitioning_to_fully_present(false)
            .with_file_megabyte(0),
        BatEntryState::Unmapped => {
            // Keep soft anchor if present.
            BlockMapping::new()
                .with_bat_state(BatEntryState::Undefined)
                .with_transitioning_to_fully_present(false)
                .with_file_megabyte(old.file_megabyte())
        }
        _ => old, // NotPresent, Undefined → no change
    }
}

/// Zero: any state → Zero (clear file offset).
fn convert_zero(state: BatEntryState, old: BlockMapping) -> BlockMapping {
    match state {
        BatEntryState::Zero if old.file_megabyte() == 0 => old, // already Zero with no offset
        _ => {
            debug_assert!(
                !old.transitioning_to_fully_present(),
                "cannot trim TFP block to Zero"
            );
            BlockMapping::new()
                .with_bat_state(BatEntryState::Zero)
                .with_transitioning_to_fully_present(false)
                .with_file_megabyte(0)
        }
    }
}

/// MakeTransparent: any state → NotPresent (clear file offset).
fn convert_make_transparent(state: BatEntryState, old: BlockMapping) -> BlockMapping {
    match state {
        BatEntryState::NotPresent if old.file_megabyte() == 0 => old, // already NotPresent
        _ => BlockMapping::new()
            .with_bat_state(BatEntryState::NotPresent)
            .with_transitioning_to_fully_present(false)
            .with_file_megabyte(0),
    }
}

/// RemoveSoftAnchors: clear file offset if soft-anchored, otherwise no-op.
fn convert_remove_soft_anchors(old: BlockMapping) -> BlockMapping {
    if old.is_soft_anchored() {
        BlockMapping::new()
            .with_bat_state(old.bat_state())
            .with_transitioning_to_fully_present(false)
            .with_file_megabyte(0)
    } else {
        old
    }
}

/// Compute the block range fully included in a byte range.
///
/// Returns `(start_block, block_count)`. Only blocks whose entire extent
/// falls within `[offset..offset+length)` are included. Leading and
/// trailing partial blocks are skipped.
fn included_blocks(offset: u64, length: u64, block_size: u64) -> (u32, u32) {
    if length == 0 {
        return (0, 0);
    }
    // First fully-included block: round UP to next block boundary.
    let start = offset.div_ceil(block_size) as u32;
    // First block NOT included: round DOWN.
    let end = ((offset + length) / block_size) as u32;
    if end <= start {
        (start, 0)
    } else {
        (start, end - start)
    }
}

impl<F: AsyncFile> VhdxFile<F> {
    /// Trim (unmap) a range of virtual disk blocks.
    ///
    /// Transitions blocks to unmapped/zero/transparent state depending on
    /// the mode specified in `request`. Only blocks fully covered by the
    /// range are trimmed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file is read-only
    /// - Offset or length is not aligned to the logical sector size
    /// - The range extends beyond the disk size (unless `skip_disk_size_check`)
    /// - The file is in a permanently failed state
    pub async fn trim(&self, request: TrimRequest) -> Result<(), VhdxIoError> {
        self.failed.check()?;

        let TrimRequest {
            mode,
            offset,
            length,
            skip_disk_size_check,
            skip_write_guid_change,
        } = request;

        // 1. Check read-only.
        if self.read_only {
            return Err(VhdxIoErrorInner::ReadOnly.into());
        }

        // 2. Zero-length — immediate success.
        if length == 0 {
            return Ok(());
        }

        // 3. Validate alignment to logical sector size.
        if !offset.is_multiple_of(self.logical_sector_size as u64)
            || !length.is_multiple_of(self.logical_sector_size as u64)
        {
            return Err(VhdxIoErrorInner::UnalignedIo.into());
        }

        // 4. Validate bounds (unless skipped).
        if !skip_disk_size_check {
            if offset
                .checked_add(length)
                .is_none_or(|end| end > self.disk_size)
            {
                return Err(VhdxIoErrorInner::BeyondEndOfDisk.into());
            }
        }

        // 5. If fully-allocated (fixed) disk and mode doesn't allow it: no-op.
        if self.is_fully_allocated() && !mode_allowed_on_fixed(mode) {
            return Ok(());
        }

        // 6. Enable write mode.
        // All trim modes modify the file (BAT entries), so FileWritable
        // is always needed. DataWritable is additionally needed when the
        // mode changes user-visible data (everything except
        // RemoveSoftAnchors) and the caller hasn't opted out.
        if !skip_write_guid_change && !mode_skips_write_guid(mode) {
            self.enable_write_mode(WriteMode::DataWritable)
                .await
                .map_err(VhdxIoErrorInner::WriteHeader)?;
        } else {
            self.enable_write_mode(WriteMode::FileWritable)
                .await
                .map_err(VhdxIoErrorInner::WriteHeader)?;
        }

        // 7. Compute effective length: if trim extends to exactly disk_size,
        //    round up to cover the full last block.
        let effective_length = if !skip_disk_size_check && offset + length == self.disk_size {
            let block_size = self.block_size as u64;
            let full_disk_size = crate::create::round_up(self.disk_size, block_size);
            full_disk_size - offset
        } else {
            length
        };

        // 8. Compute included blocks.
        let (start_block, block_count) =
            included_blocks(offset, effective_length, self.block_size as u64);
        if block_count == 0 {
            return Ok(());
        }
        let end_block = start_block + block_count;

        // 9. Main trim loop.
        //
        // For each block, we atomically claim it (CAS 0 → SENTINEL),
        // preventing any new I/O from reading stale mappings. Then we
        // read + convert the mapping, write the BAT, handle space
        // management, and release the claim.
        let mut current_block = start_block;
        loop {
            if current_block >= end_block {
                return Ok(());
            }

            // 9a. Claim the block: set trim-pending to block new I/O,
            //     wait for in-flight I/Os to drain, then take exclusive
            //     ownership.
            let claim = self.bat.claim_for_trim(current_block).await;

            // 9b. Block is claimed — no new I/O can start on it.
            //     Read the mapping and compute the trim conversion.
            let old_mapping = self.bat.get_block_mapping(current_block);
            let new_mapping = convert_mapping(mode, old_mapping);

            if old_mapping == new_mapping {
                // No-op — release claim and advance.
                current_block += 1;
                continue;
            }

            // 9c. Write BAT entry to cache (also updates in-memory BAT atomically).
            // LOCK AUDIT: Trim claim held (not a sync lock). Safe to await.
            self.bat
                .write_block_mapping(
                    &self.cache,
                    BlockType::Payload,
                    current_block,
                    new_mapping,
                    None,
                )
                .await?;

            // 9e. Handle space management based on old→new transition.
            //
            // Space releases are deferred until the BAT change is durable
            // on disk. Without deferral, a crash could teleport data from
            // a new block into the old block's offset.
            let old_anchored = old_mapping.is_soft_anchored();
            let new_anchored = new_mapping.is_soft_anchored();
            let old_file_mb = old_mapping.file_megabyte();
            let new_file_mb = new_mapping.file_megabyte();
            let old_file_offset = old_file_mb as u64 * MB1;
            let block_size = self.block_size;

            if old_anchored && new_anchored {
                // Same anchor — assert same file offset, no space management.
                debug_assert_eq!(old_file_mb, new_file_mb);
            } else if old_anchored && !new_anchored {
                // Was soft-anchored → no longer: unmark/cancel + defer release.
                let was_deferred = self.deferred_releases.cancel(current_block);
                if !was_deferred {
                    assert!(
                        self.free_space.unmark_trimmed_block(
                            current_block,
                            old_file_offset,
                            block_size,
                        ),
                        "soft-anchored block {current_block} not tracked as trimmed"
                    );
                }
                self.deferred_releases
                    .insert(current_block, old_file_offset, block_size, false);
            } else if !old_anchored && new_anchored {
                // Was not anchored → now soft-anchored: defer the anchor.
                self.deferred_releases
                    .insert(current_block, old_file_offset, block_size, true);
            } else {
                // Neither was nor becomes anchored.
                if old_file_mb != 0 {
                    self.deferred_releases.insert(
                        current_block,
                        old_file_offset,
                        block_size,
                        false,
                    );
                }
            }

            // 9f. Release the trim claim — I/O can resume on this block.
            drop(claim);

            // Quota check: force flush if too many deferred releases.
            if self.deferred_releases.needs_flush() {
                self.flush().await?;
            }

            current_block += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- included_blocks unit tests ----

    #[test]
    fn included_blocks_full_coverage() {
        // Range exactly covers blocks 0..3 (3 blocks).
        let block_size = 2 * MB1;
        let (start, count) = included_blocks(0, 3 * block_size, block_size);
        assert_eq!(start, 0);
        assert_eq!(count, 3);
    }

    #[test]
    fn included_blocks_partial_edges() {
        // Start mid-block-0, end mid-block-2 → only block 1 included.
        let block_size = 2 * MB1;
        let (start, count) = included_blocks(MB1, 2 * block_size, block_size);
        assert_eq!(start, 1); // block 0 is partial
        assert_eq!(count, 1); // only block 1 fully covered
    }

    #[test]
    fn included_blocks_zero_length() {
        let (start, count) = included_blocks(0, 0, 2 * MB1);
        assert_eq!(start, 0);
        assert_eq!(count, 0);
    }

    #[test]
    fn included_blocks_too_small() {
        // Range is less than one block → no blocks included.
        let block_size = 2 * MB1;
        let (_start, count) = included_blocks(MB1, MB1, block_size);
        assert_eq!(count, 0);
    }

    // ---- Conversion function unit tests ----

    #[test]
    fn convert_file_space_mappings() {
        // FullyPresent → Unmapped (keep offset)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte(4);
        let r = convert_mapping(TrimMode::FileSpace, m);
        assert_eq!(r.bat_state(), BatEntryState::Unmapped);
        assert_eq!(r.file_megabyte(), 4);

        // Undefined → Undefined (no change)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Undefined)
            .with_file_megabyte(0);
        let r = convert_mapping(TrimMode::FileSpace, m);
        assert_eq!(r, m);

        // Unmapped → Unmapped (no change)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Unmapped)
            .with_file_megabyte(5);
        let r = convert_mapping(TrimMode::FileSpace, m);
        assert_eq!(r, m);
    }

    #[test]
    fn convert_free_space_mappings() {
        // FullyPresent → Undefined (clear offset)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte(4);
        let r = convert_mapping(TrimMode::FreeSpace, m);
        assert_eq!(r.bat_state(), BatEntryState::Undefined);
        assert_eq!(r.file_megabyte(), 0);

        // Unmapped → Undefined (keep anchor)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Unmapped)
            .with_file_megabyte(5);
        let r = convert_mapping(TrimMode::FreeSpace, m);
        assert_eq!(r.bat_state(), BatEntryState::Undefined);
        assert_eq!(r.file_megabyte(), 5);
    }

    #[test]
    fn convert_zero_mappings() {
        // FullyPresent → Zero (clear offset)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte(4);
        let r = convert_mapping(TrimMode::Zero, m);
        assert_eq!(r.bat_state(), BatEntryState::Zero);
        assert_eq!(r.file_megabyte(), 0);

        // Zero (no offset) → Zero (no change)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Zero)
            .with_file_megabyte(0);
        let r = convert_mapping(TrimMode::Zero, m);
        assert_eq!(r, m);
    }

    #[test]
    fn convert_make_transparent_mappings() {
        // FullyPresent → NotPresent
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte(4);
        let r = convert_mapping(TrimMode::MakeTransparent, m);
        assert_eq!(r.bat_state(), BatEntryState::NotPresent);
        assert_eq!(r.file_megabyte(), 0);

        // NotPresent → NotPresent (no change)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::NotPresent)
            .with_file_megabyte(0);
        let r = convert_mapping(TrimMode::MakeTransparent, m);
        assert_eq!(r, m);
    }

    #[test]
    fn convert_remove_soft_anchors_mappings() {
        // Unmapped with offset → clear offset
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Unmapped)
            .with_file_megabyte(5);
        let r = convert_mapping(TrimMode::RemoveSoftAnchors, m);
        assert_eq!(r.bat_state(), BatEntryState::Unmapped);
        assert_eq!(r.file_megabyte(), 0);

        // FullyPresent → no change (not soft-anchored)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte(4);
        let r = convert_mapping(TrimMode::RemoveSoftAnchors, m);
        assert_eq!(r, m);
    }

    #[test]
    fn is_soft_anchored_checks() {
        // Unmapped with offset → anchored
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Unmapped)
            .with_file_megabyte(5);
        assert!(m.is_soft_anchored());

        // Undefined with offset → anchored
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Undefined)
            .with_file_megabyte(3);
        assert!(m.is_soft_anchored());

        // Unmapped with no offset → not anchored
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::Unmapped)
            .with_file_megabyte(0);
        assert!(!m.is_soft_anchored());

        // FullyPresent with offset → not anchored (wrong state)
        let m = BlockMapping::new()
            .with_bat_state(BatEntryState::FullyPresent)
            .with_file_megabyte(4);
        assert!(!m.is_soft_anchored());
    }
}
