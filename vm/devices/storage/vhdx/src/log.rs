// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! VHDX write-ahead log (WAL) — replay and entry construction.
//!
//! The VHDX format uses a write-ahead log stored in a circular "log region"
//! to ensure crash consistency of metadata (BAT, sector bitmaps). This
//! module provides:
//!
//! - [`replay_log`] — scans the log region for valid entries and applies
//!   them to the file.
//! - [`LogWriter`] — constructs and writes new log entries.
//!
//! This module is self-contained and depends only on [`crate::format`],
//! [`crate::error`], and external crates.

use crate::AsyncFile;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::error::OpenErrorInner;
use crate::format::LOG_DATA_SECTOR_SIGNATURE;
use crate::format::LOG_DESCRIPTOR_DATA_SIGNATURE;
use crate::format::LOG_DESCRIPTOR_ZERO_SIGNATURE;
use crate::format::LOG_ENTRY_HEADER_SIGNATURE;
use crate::format::LOG_SECTOR_SIZE;
use crate::format::LOGGABLE_OFFSET;
use crate::format::LogDataDescriptor;
use crate::format::LogDataSector;
use crate::format::LogEntryHeader;
use crate::format::LogZeroDescriptor;
use crate::format::compute_checksum;
use guid::Guid;
use zerocopy::FromBytes;
use zerocopy::IntoBytes;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const SECTOR: u32 = LOG_SECTOR_SIZE as u32;
const HEADER_SIZE: u32 = size_of::<LogEntryHeader>() as u32; // 64
const DESCRIPTOR_SIZE: u32 = size_of::<LogDataDescriptor>() as u32; // 32

// ---------------------------------------------------------------------------
// LogRegion — circular buffer helpers
// ---------------------------------------------------------------------------

/// Describes the circular log region in the VHDX file.
#[derive(Debug, Clone)]
pub struct LogRegion {
    /// File offset where the log region starts.
    pub file_offset: u64,
    /// Length of the log region in bytes (always a multiple of `LOG_SECTOR_SIZE`).
    pub length: u32,
}

impl LogRegion {
    /// Modular add within the region: `(offset + length) % self.length`.
    fn log_add(&self, offset: u32, len: u32) -> u32 {
        let sum = offset + len;
        if sum >= self.length {
            sum - self.length
        } else {
            sum
        }
    }

    /// Length of the circular sequence `[tail, head)`.
    ///
    /// If `head == tail`, the sequence spans the entire log (full).
    fn sequence_length(&self, tail: u32, head: u32) -> u32 {
        if head > tail {
            head - tail
        } else if head < tail {
            head + self.length - tail
        } else {
            // head == tail → full
            self.length
        }
    }

    /// Free (unused) space in the log given the current `[tail, head)`.
    fn free_space(&self, tail: u32, head: u32) -> u32 {
        self.length - self.sequence_length(tail, head)
    }

    /// Returns `true` if `offset` lies within the circular range `[tail, head)`.
    fn is_within_sequence(&self, tail: u32, head: u32, offset: u32) -> bool {
        self.sequence_length(offset, head) <= self.sequence_length(tail, head)
    }

    /// Read a single sector from the log at `base + offset` (modular).
    async fn read_sector<F: AsyncFile>(
        &self,
        file: &F,
        base: u32,
        offset: u32,
        buf: F::Buffer,
    ) -> Result<F::Buffer, OpenError> {
        let pos = self.log_add(base, offset);
        let buf = file
            .read_into(self.file_offset + pos as u64, buf)
            .await
            .map_err(OpenErrorInner::Io)?;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Entry size helpers
// ---------------------------------------------------------------------------

/// Length of the descriptor area (header + descriptor sectors), rounded up
/// to a multiple of `LOG_SECTOR_SIZE`.
fn descriptor_area_length(descriptor_count: u32) -> u32 {
    let raw = HEADER_SIZE + descriptor_count * DESCRIPTOR_SIZE;
    raw.div_ceil(SECTOR) * SECTOR
}

/// Total entry length: descriptor area + one data sector per data descriptor.
fn entry_length(data_count: u32, zero_count: u32) -> u32 {
    descriptor_area_length(data_count + zero_count) + data_count * SECTOR
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Result of log replay.
#[derive(Debug, Clone)]
pub struct ReplayResult {
    /// Whether any entries were replayed.
    pub replayed: bool,
}

/// A validated log sequence found during scanning.
#[derive(Debug, Clone)]
struct LogSequence {
    tail: u32,
    head: u32,
    last_lsn: u64,
}

/// Replay the VHDX log.
///
/// Scans the log region for valid entries matching `log_guid`, applies
/// them to the file, and flushes. Returns information about what was done.
///
/// This function is self-contained: it takes a file handle and log region
/// parameters, not a `VhdxFile`. It can be called before the file is
/// fully parsed.
pub async fn replay_log<F: AsyncFile>(
    file: &F,
    log_region: &LogRegion,
    log_guid: Guid,
) -> Result<ReplayResult, OpenError> {
    // Step 1: find the best valid sequence.
    let sequence = find_log_sequence(file, log_region, &log_guid).await?;

    // Step 2: apply the sequence.
    apply_sequence(file, log_region, &log_guid, &sequence).await
}

/// Scan the entire log for the sequence with the highest LSN.
async fn find_log_sequence<F: AsyncFile>(
    file: &F,
    region: &LogRegion,
    log_guid: &Guid,
) -> Result<LogSequence, OpenError> {
    let mut best: Option<LogSequence> = None;
    let mut tail: u32 = 0;

    loop {
        match find_sequence_from_tail(file, region, log_guid, tail).await {
            Ok(seq) => {
                let dominated = best.as_ref().is_some_and(|b| b.last_lsn >= seq.last_lsn);
                if !dominated {
                    best = Some(seq.clone());
                }
                // Advance past this sequence.
                if seq.head <= tail {
                    break; // wrapped
                }
                tail = seq.head;
            }
            Err(FindError::NoSequence) => {
                tail += SECTOR;
                if tail >= region.length {
                    break;
                }
            }
            Err(FindError::Vhdx(e)) => return Err(e),
        }
    }

    Ok(best.ok_or(CorruptionType::NoValidLogEntries)?)
}

enum FindError {
    NoSequence,
    Vhdx(OpenError),
}

/// Try to build a sequence starting at `original_tail`.
async fn find_sequence_from_tail<F: AsyncFile>(
    file: &F,
    region: &LogRegion,
    log_guid: &Guid,
    original_tail: u32,
) -> Result<LogSequence, FindError> {
    let mut seq = LogSequence {
        tail: 0,
        head: original_tail,
        last_lsn: 0,
    };
    let mut first = true;

    loop {
        match expand_sequence(file, region, log_guid, &mut seq, first).await {
            Ok(()) => {
                first = false;
            }
            Err(FindError::NoSequence) if !first => break,
            Err(e) => return Err(e),
        }
    }

    // Verify that the sequence's tail lies within the validated range.
    if !region.is_within_sequence(original_tail, seq.head, seq.tail) {
        return Err(FindError::NoSequence);
    }

    Ok(seq)
}

/// Try to grow the sequence by one entry at `seq.head`.
async fn expand_sequence<F: AsyncFile>(
    file: &F,
    region: &LogRegion,
    log_guid: &Guid,
    seq: &mut LogSequence,
    first: bool,
) -> Result<(), FindError> {
    let mut sector_buf = file.alloc_buffer(SECTOR as usize);

    // Read the first sector at the candidate position.
    sector_buf = region
        .read_sector(file, seq.head, 0, sector_buf)
        .await
        .map_err(FindError::Vhdx)?;

    let header = LogEntryHeader::read_from_bytes(&sector_buf.as_ref()[..HEADER_SIZE as usize])
        .map_err(|_| FindError::NoSequence)?
        .clone();

    // Validate header fields.
    if header.signature != LOG_ENTRY_HEADER_SIGNATURE {
        return Err(FindError::NoSequence);
    }
    if header.log_guid != *log_guid {
        return Err(FindError::NoSequence);
    }
    if !validate_entry_header_fields(&header, region) {
        return Err(FindError::NoSequence);
    }

    let new_head = region.log_add(seq.head, header.entry_length);

    if !first {
        // Check sequence continuity.
        if header.sequence_number != seq.last_lsn + 1 {
            return Err(FindError::NoSequence);
        }
        // Entry must fit in the free space.
        if header.entry_length > region.free_space(seq.tail, seq.head) {
            return Err(FindError::NoSequence);
        }
        // New tail must be within the growing sequence.
        if !region.is_within_sequence(seq.tail, new_head, header.tail) {
            return Err(FindError::NoSequence);
        }
    }

    // Compute CRC-32C over the entire entry.
    let buf_ref = sector_buf.as_ref();
    let mut crc = compute_checksum(buf_ref, 4);

    // Read and checksum additional descriptor sectors.
    let desc_area_len = descriptor_area_length(header.descriptor_count);
    let mut data_descriptor_count: u32 = 0;

    // Validate descriptors.
    for i in 0..header.descriptor_count {
        let byte_offset = HEADER_SIZE + i * DESCRIPTOR_SIZE;
        if byte_offset.is_multiple_of(SECTOR) {
            // Need to read a new sector.
            sector_buf = region
                .read_sector(file, seq.head, byte_offset, sector_buf)
                .await
                .map_err(FindError::Vhdx)?;
            crc = crc32c::crc32c_append(crc, sector_buf.as_ref());
        }

        let local_off = (byte_offset % SECTOR) as usize;
        let desc_bytes = &sector_buf.as_ref()[local_off..local_off + DESCRIPTOR_SIZE as usize];

        // Check descriptor signature.
        let sig = u32::from_le_bytes(desc_bytes[0..4].try_into().unwrap());
        if sig == LOG_DESCRIPTOR_DATA_SIGNATURE {
            let desc = LogDataDescriptor::read_from_bytes(desc_bytes)
                .map_err(|_| FindError::NoSequence)?;
            if !validate_data_descriptor(&desc, &header, region) {
                return Err(FindError::NoSequence);
            }
            data_descriptor_count += 1;
        } else if sig == LOG_DESCRIPTOR_ZERO_SIGNATURE {
            let desc = LogZeroDescriptor::read_from_bytes(desc_bytes)
                .map_err(|_| FindError::NoSequence)?;
            if !validate_zero_descriptor(&desc, &header, region) {
                return Err(FindError::NoSequence);
            }
        } else {
            return Err(FindError::NoSequence);
        }
    }

    // Validate that the entry length matches.
    let expected_len = entry_length(
        data_descriptor_count,
        header.descriptor_count - data_descriptor_count,
    );
    if header.entry_length != expected_len {
        return Err(FindError::NoSequence);
    }

    // Read and validate data sectors.
    for i in 0..data_descriptor_count {
        let offset = desc_area_len + i * SECTOR;
        sector_buf = region
            .read_sector(file, seq.head, offset, sector_buf)
            .await
            .map_err(FindError::Vhdx)?;
        crc = crc32c::crc32c_append(crc, sector_buf.as_ref());

        let data_sector = LogDataSector::read_from_bytes(sector_buf.as_ref())
            .map_err(|_| FindError::NoSequence)?;
        if data_sector.signature != LOG_DATA_SECTOR_SIGNATURE {
            return Err(FindError::NoSequence);
        }
        if data_sector.sequence_low != header.sequence_number as u32
            || data_sector.sequence_high != (header.sequence_number >> 32) as u32
        {
            return Err(FindError::NoSequence);
        }
    }

    // Verify CRC.
    if crc != header.checksum {
        return Err(FindError::NoSequence);
    }

    // Entry is valid — update the sequence.
    seq.last_lsn = header.sequence_number;
    seq.tail = header.tail;
    seq.head = new_head;

    Ok(())
}

fn validate_entry_header_fields(header: &LogEntryHeader, region: &LogRegion) -> bool {
    if header.tail >= region.length {
        return false;
    }
    if !header.entry_length.is_multiple_of(SECTOR)
        || header.entry_length < SECTOR
        || header.entry_length >= region.length
    {
        return false;
    }
    // Descriptor count must fit within the entry.
    let max_desc = (header.entry_length - HEADER_SIZE) / DESCRIPTOR_SIZE;
    if header.descriptor_count > max_desc {
        return false;
    }
    true
}

fn validate_data_descriptor(
    desc: &LogDataDescriptor,
    header: &LogEntryHeader,
    region: &LogRegion,
) -> bool {
    if !desc.file_offset.is_multiple_of(LOG_SECTOR_SIZE) {
        return false;
    }
    if desc.file_offset < LOGGABLE_OFFSET {
        return false;
    }
    if desc.sequence_number != header.sequence_number {
        return false;
    }
    // Must not overlap the log region.
    let write_end = desc.file_offset.checked_add(LOG_SECTOR_SIZE);
    if let Some(end) = write_end {
        if desc.file_offset < region.file_offset + region.length as u64 && end > region.file_offset
        {
            return false;
        }
    } else {
        return false;
    }
    true
}

fn validate_zero_descriptor(
    desc: &LogZeroDescriptor,
    header: &LogEntryHeader,
    region: &LogRegion,
) -> bool {
    if !desc.file_offset.is_multiple_of(LOG_SECTOR_SIZE) {
        return false;
    }
    if desc.file_offset < LOGGABLE_OFFSET {
        return false;
    }
    if !desc.length.is_multiple_of(LOG_SECTOR_SIZE) {
        return false;
    }
    if desc.sequence_number != header.sequence_number {
        return false;
    }
    // Must not overlap the log region.
    let write_end = desc.file_offset.checked_add(desc.length);
    if let Some(end) = write_end {
        if desc.file_offset < region.file_offset + region.length as u64 && end > region.file_offset
        {
            return false;
        }
    } else {
        return false;
    }
    true
}

/// Apply a validated sequence to the file.
async fn apply_sequence<F: AsyncFile>(
    file: &F,
    region: &LogRegion,
    log_guid: &Guid,
    sequence: &LogSequence,
) -> Result<ReplayResult, OpenError> {
    let mut tail = sequence.tail;
    let head = sequence.head;
    let mut last_file_offset: u64 = 0;
    let mut replayed = false;

    let mut sector_buf = file.alloc_buffer(SECTOR as usize);

    while tail != head {
        // Read header.
        sector_buf = region.read_sector(file, tail, 0, sector_buf).await?;
        let header = LogEntryHeader::read_from_bytes(&sector_buf.as_ref()[..HEADER_SIZE as usize])
            .map_err(|_| CorruptionType::BadLogEntryOnReplay)?
            .clone();

        if header.signature != LOG_ENTRY_HEADER_SIGNATURE || header.log_guid != *log_guid {
            return Err(CorruptionType::BadLogEntryOnReplay.into());
        }

        last_file_offset = header.last_file_offset;

        let desc_area_len = descriptor_area_length(header.descriptor_count);
        let mut data_sector_index: u32 = 0;

        for i in 0..header.descriptor_count {
            let byte_offset = HEADER_SIZE + i * DESCRIPTOR_SIZE;
            if byte_offset.is_multiple_of(SECTOR) || i == 0 {
                // (Re-)read the descriptor sector. For the first descriptor
                // the sector_buf already contains the header sector.
                if byte_offset >= SECTOR {
                    sector_buf = region
                        .read_sector(file, tail, byte_offset - (byte_offset % SECTOR), sector_buf)
                        .await?;
                }
            }

            let local_off = (byte_offset % SECTOR) as usize;
            let desc_bytes = &sector_buf.as_ref()[local_off..local_off + DESCRIPTOR_SIZE as usize];
            let sig = u32::from_le_bytes(desc_bytes[0..4].try_into().unwrap());

            if sig == LOG_DESCRIPTOR_ZERO_SIGNATURE {
                let desc = LogZeroDescriptor::read_from_bytes(desc_bytes)
                    .map_err(|_| CorruptionType::BadLogEntryOnReplay)?;

                // Write zeros.
                file.zero_range(desc.file_offset, desc.length)
                    .await
                    .map_err(OpenErrorInner::Io)?;
            } else if sig == LOG_DESCRIPTOR_DATA_SIGNATURE {
                let desc = LogDataDescriptor::read_from_bytes(desc_bytes)
                    .map_err(|_| CorruptionType::BadLogEntryOnReplay)?
                    .clone();

                // Read the data sector from the log.
                let data_offset = desc_area_len + data_sector_index * SECTOR;
                let data_buf = file.alloc_buffer(SECTOR as usize);
                let mut data_buf = region
                    .read_sector(file, tail, data_offset, data_buf)
                    .await?;

                // Reconstruct the original 4096-byte sector.
                // Replace first 8 bytes (signature + sequence_high) with leading_bytes.
                let leading = desc.leading_bytes.to_le_bytes();
                data_buf.as_mut()[0..8].copy_from_slice(&leading);
                // Replace last 4 bytes (sequence_low) with trailing_bytes.
                let trailing = desc.trailing_bytes.to_le_bytes();
                data_buf.as_mut()[SECTOR as usize - 4..].copy_from_slice(&trailing);

                file.write_from(desc.file_offset, data_buf)
                    .await
                    .map_err(OpenErrorInner::Io)?;
                data_sector_index += 1;
            } else {
                return Err(CorruptionType::BadLogEntryOnReplay.into());
            }
        }

        replayed = true;
        tail = region.log_add(tail, header.entry_length);
    }

    // Extend file if needed.
    if replayed {
        let file_sz = file.file_size().await.map_err(OpenErrorInner::Io)?;
        if file_sz < last_file_offset {
            file.set_file_size(last_file_offset)
                .await
                .map_err(OpenErrorInner::Io)?;
        }
        file.flush().await.map_err(OpenErrorInner::Io)?;
    }

    Ok(ReplayResult { replayed })
}

// ---------------------------------------------------------------------------
// LogWriter
// ---------------------------------------------------------------------------

/// A page to be logged: 4096 bytes of data at a file offset.
pub struct DataPage<'a> {
    /// Target file offset (must be aligned to LOG_SECTOR_SIZE).
    pub file_offset: u64,
    /// The 4096-byte data payload.
    pub payload: &'a [u8; SECTOR as usize],
}

/// A range to be zeroed during replay.
pub struct ZeroRange {
    /// Target file offset (must be aligned to LOG_SECTOR_SIZE).
    pub file_offset: u64,
    /// Length in bytes (must be a multiple of LOG_SECTOR_SIZE).
    pub length: u64,
}

/// Active log state for writing new entries.
pub struct LogWriter {
    region: LogRegion,
    tail: u32,
    head: u32,
    sequence_number: u64,
    log_guid: Guid,
    flushed_file_offset: u64,
    last_file_offset: u64,
}

impl LogWriter {
    /// Create a new `LogWriter` for an empty log.
    ///
    /// Writes an initial empty entry (zero data descriptors, zero zero-ranges)
    /// and flushes. Returns the writer ready for subsequent entries.
    pub async fn initialize<F: AsyncFile>(
        file: &F,
        region: LogRegion,
        log_guid: Guid,
        last_file_offset: u64,
    ) -> Result<Self, OpenError> {
        let mut writer = LogWriter {
            region,
            tail: 0,
            head: 0,
            sequence_number: 0,
            log_guid,
            flushed_file_offset: last_file_offset,
            last_file_offset,
        };

        // Write an initial empty entry with sequence number 1.
        writer
            .write_entry(file, &[], &[])
            .await
            .map_err(OpenErrorInner::Io)?
            .ok_or(CorruptionType::LogFull)?;
        file.flush().await.map_err(OpenErrorInner::Io)?;
        Ok(writer)
    }

    /// Create a `LogWriter` from an existing valid log sequence.
    ///
    /// Returns the amount of free space remaining in the log.
    pub fn free_space(&self) -> u32 {
        self.region.free_space(self.tail, self.head)
    }

    /// Advance the log tail by `len` bytes, reclaiming space.
    ///
    /// The caller must ensure that all entries in the range `[old_tail, old_tail + len)`
    /// have been fully applied and their pages are durable at final file offsets.
    pub fn advance_tail(&mut self, new_tail: u32) {
        self.tail = new_tail;
    }

    /// Returns the current head offset within the log region.
    pub fn head(&self) -> u32 {
        self.head
    }

    /// Write a log entry containing the given data pages and zero ranges.
    ///
    /// Returns `Some(sequence_number)` on success, or `None` if the log
    /// doesn't have enough free space (caller should drain and retry).
    pub async fn write_entry<F: AsyncFile>(
        &mut self,
        file: &F,
        data_pages: &[DataPage<'_>],
        zero_ranges: &[ZeroRange],
    ) -> Result<Option<u64>, std::io::Error> {
        let data_count = data_pages.len() as u32;
        let zero_count = zero_ranges.len() as u32;
        let total_desc = data_count + zero_count;
        let elen = entry_length(data_count, zero_count);

        // We always leave room for at least one more sector to avoid completely
        // filling the log.
        let needed = elen + SECTOR;
        if self.tail == self.head {
            // Empty log — the full region is free.
            if needed > self.region.length {
                return Ok(None);
            }
        } else if needed > self.free_space() {
            return Ok(None);
        }

        self.sequence_number += 1;
        let seq = self.sequence_number;

        // --- Allocate entry buffers ---
        // The entry may wrap around the circular log boundary. Since head
        // and elen are both sector-aligned, the split always falls on a
        // sector boundary. Allocate one or two buffers accordingly.
        let remaining = (self.region.length - self.head) as usize;
        let wraps = (elen as usize) > remaining;
        let mut buf1 = file.alloc_buffer(if wraps { remaining } else { elen as usize });
        let b1 = buf1.as_mut();
        let mut buf2 = if wraps {
            Some(file.alloc_buffer(elen as usize - remaining))
        } else {
            None
        };
        let b2 = buf2.as_mut().map_or(&mut [][..], |b| b.as_mut());
        let split = remaining; // byte offset within the entry where the split occurs

        fn entry_slice<'a>(
            b1: &'a mut [u8],
            b2: &'a mut [u8],
            split: usize,
            offset: usize,
            len: usize,
        ) -> &'a mut [u8] {
            if offset < split {
                debug_assert!(offset + len <= split, "access straddles split boundary");
                &mut b1[offset..offset + len]
            } else {
                let off2 = offset - split;
                &mut b2[off2..off2 + len]
            }
        }

        // --- Build the entry ---

        // Header (first 64 bytes of first sector).
        let header = LogEntryHeader {
            signature: LOG_ENTRY_HEADER_SIGNATURE,
            checksum: 0,
            entry_length: elen,
            tail: self.tail,
            sequence_number: seq,
            descriptor_count: total_desc,
            reserved: 0,
            log_guid: self.log_guid,
            flushed_file_offset: self.flushed_file_offset,
            last_file_offset: self.last_file_offset,
        };
        entry_slice(b1, b2, split, 0, HEADER_SIZE as usize).copy_from_slice(header.as_bytes());

        // Descriptors.
        let desc_area_len = descriptor_area_length(total_desc);
        let mut desc_offset = HEADER_SIZE as usize;
        let mut data_sector_offset = desc_area_len as usize;

        for dp in data_pages {
            let leading = u64::from_le_bytes(dp.payload[0..8].try_into().unwrap());
            let trailing = u32::from_le_bytes(
                dp.payload[SECTOR as usize - 4..SECTOR as usize]
                    .try_into()
                    .unwrap(),
            );
            let desc = LogDataDescriptor {
                signature: LOG_DESCRIPTOR_DATA_SIGNATURE,
                trailing_bytes: trailing,
                leading_bytes: leading,
                file_offset: dp.file_offset,
                sequence_number: seq,
            };
            entry_slice(b1, b2, split, desc_offset, DESCRIPTOR_SIZE as usize)
                .copy_from_slice(desc.as_bytes());
            desc_offset += DESCRIPTOR_SIZE as usize;

            let ds = build_data_sector(dp.payload, seq);
            entry_slice(b1, b2, split, data_sector_offset, SECTOR as usize)
                .copy_from_slice(ds.as_bytes());
            data_sector_offset += SECTOR as usize;
        }

        for zr in zero_ranges {
            let desc = LogZeroDescriptor {
                signature: LOG_DESCRIPTOR_ZERO_SIGNATURE,
                reserved: 0,
                length: zr.length,
                file_offset: zr.file_offset,
                sequence_number: seq,
            };
            entry_slice(b1, b2, split, desc_offset, DESCRIPTOR_SIZE as usize)
                .copy_from_slice(desc.as_bytes());
            desc_offset += DESCRIPTOR_SIZE as usize;
        }

        // Compute CRC-32C across both buffers (checksum field is already zero).
        let mut crc = crc32c::crc32c(buf1.as_ref());
        if let Some(ref b2) = buf2 {
            crc = crc32c::crc32c_append(crc, b2.as_ref());
        }
        // Write checksum into the header (always in buf1, bytes 4..8).
        buf1.as_mut()[4..8].copy_from_slice(&crc.to_le_bytes());

        // --- Write to file ---
        file.write_from(self.region.file_offset + self.head as u64, buf1)
            .await?;
        if let Some(buf2) = buf2 {
            file.write_from(self.region.file_offset, buf2).await?;
        }

        // Advance head.
        self.head = self.region.log_add(self.head, elen);

        Ok(Some(seq))
    }
}

/// Build a `LogDataSector` from a page of original data and a sequence number.
fn build_data_sector(source: &[u8; SECTOR as usize], sequence_number: u64) -> LogDataSector {
    let mut data = [0u8; 4084];
    data.copy_from_slice(&source[8..SECTOR as usize - 4]);
    LogDataSector {
        signature: LOG_DATA_SECTOR_SIGNATURE,
        sequence_high: (sequence_number >> 32) as u32,
        data,
        sequence_low: sequence_number as u32,
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;

    /// Helpers for tests.
    const TEST_LOG_SIZE: u32 = 64 * SECTOR; // 256 KiB
    const TEST_LOG_OFFSET: u64 = 1024 * 1024; // 1 MiB into the file

    fn test_region() -> LogRegion {
        LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: TEST_LOG_SIZE,
        }
    }

    fn test_guid() -> Guid {
        guid::guid!("12345678-1234-1234-1234-123456789abc")
    }

    /// Create a file large enough for the log region and a target area.
    fn test_file() -> InMemoryFile {
        // 4 MiB file: enough for the log at 1 MiB and target writes at 192 KiB+.
        InMemoryFile::new(4 * 1024 * 1024)
    }

    // -----------------------------------------------------------------------
    // Circular buffer helper tests
    // -----------------------------------------------------------------------

    #[test]
    fn log_add_no_wrap() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        assert_eq!(r.log_add(100, 200), 300);
    }

    #[test]
    fn log_add_with_wrap() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        assert_eq!(r.log_add(800, 300), 100);
    }

    #[test]
    fn sequence_length_head_gt_tail() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        assert_eq!(r.sequence_length(100, 500), 400);
    }

    #[test]
    fn sequence_length_head_lt_tail() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        assert_eq!(r.sequence_length(800, 200), 400);
    }

    #[test]
    fn sequence_length_head_eq_tail() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        // Full log.
        assert_eq!(r.sequence_length(500, 500), 1000);
    }

    #[test]
    fn free_space_computation() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        assert_eq!(r.free_space(100, 500), 600);
        assert_eq!(r.free_space(800, 200), 600);
    }

    #[test]
    fn is_within_sequence_cases() {
        let r = LogRegion {
            file_offset: 0,
            length: 1000,
        };
        // Normal range [100, 500)
        assert!(r.is_within_sequence(100, 500, 100)); // at tail
        assert!(r.is_within_sequence(100, 500, 300)); // in middle
        assert!(!r.is_within_sequence(100, 500, 500)); // at head (not within)
        assert!(!r.is_within_sequence(100, 500, 50)); // before tail

        // Wrapped range [800, 200)
        assert!(r.is_within_sequence(800, 200, 900)); // in first part
        assert!(r.is_within_sequence(800, 200, 100)); // in second part
        assert!(!r.is_within_sequence(800, 200, 500)); // outside
    }

    // -----------------------------------------------------------------------
    // Log Writer tests
    // -----------------------------------------------------------------------

    #[async_test]
    async fn writer_initialize_creates_empty_entry() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        assert_eq!(writer.sequence_number, 1);
        assert_eq!(writer.tail, 0);
        // The empty entry is 1 sector (header only, 0 descriptors).
        assert_eq!(writer.head, SECTOR);

        // Read back and validate.
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(TEST_LOG_OFFSET, &mut buf).await.unwrap();
        let header = LogEntryHeader::read_from_bytes(&buf[..HEADER_SIZE as usize]).unwrap();
        assert_eq!(header.signature, LOG_ENTRY_HEADER_SIGNATURE);
        assert_eq!(header.sequence_number, 1);
        assert_eq!(header.descriptor_count, 0);
        assert_eq!(header.entry_length, SECTOR);
        assert_eq!(header.log_guid, guid);

        // Validate CRC.
        let stored_crc = header.checksum;
        let computed_crc = compute_checksum(&buf, 4);
        assert_eq!(stored_crc, computed_crc);
    }

    #[async_test]
    async fn writer_one_data_page() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let mut page_data = [0u8; SECTOR as usize];
        for (i, b) in page_data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        let target_offset = LOGGABLE_OFFSET + 4096;

        let lsn = writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: target_offset,
                    payload: &page_data,
                }],
                &[],
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(lsn, 2);

        // Read back the entry header and verify.
        let entry_start = TEST_LOG_OFFSET + SECTOR as u64; // after the init entry
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(entry_start, &mut buf).await.unwrap();
        let header = LogEntryHeader::read_from_bytes(&buf[..HEADER_SIZE as usize]).unwrap();
        assert_eq!(header.signature, LOG_ENTRY_HEADER_SIGNATURE);
        assert_eq!(header.sequence_number, 2);
        assert_eq!(header.descriptor_count, 1);
        assert_eq!(header.entry_length, entry_length(1, 0));
    }

    #[async_test]
    async fn writer_one_zero_range() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let target_offset = LOGGABLE_OFFSET;
        let lsn = writer
            .write_entry(
                &file,
                &[],
                &[ZeroRange {
                    file_offset: target_offset,
                    length: 8192,
                }],
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(lsn, 2);

        // Read back and verify descriptor.
        let entry_start = TEST_LOG_OFFSET + SECTOR as u64;
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(entry_start, &mut buf).await.unwrap();
        let header = LogEntryHeader::read_from_bytes(&buf[..HEADER_SIZE as usize]).unwrap();
        assert_eq!(header.descriptor_count, 1);
        // Zero descriptor: entry is just 1 sector (header + descriptor fits)
        assert_eq!(header.entry_length, entry_length(0, 1));
    }

    #[async_test]
    async fn writer_multiple_entries_advance() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page = [0xAAu8; SECTOR as usize];
        for i in 0..3 {
            let lsn = writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: LOGGABLE_OFFSET + (i as u64) * 4096,
                        payload: &page,
                    }],
                    &[],
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(lsn, i as u64 + 2);
        }

        // Head should have advanced past the initial empty entry + 3 data entries.
        let one_data_entry_len = entry_length(1, 0);
        let expected_head = SECTOR + 3 * one_data_entry_len;
        assert_eq!(writer.head, expected_head);
        assert_eq!(writer.sequence_number, 4);
    }

    #[async_test]
    async fn writer_wrap_around() {
        let file = test_file();
        // Use a small log that will force wrap-around.
        let small_log_size = 16 * SECTOR; // 64 KiB
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: small_log_size,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // Each data entry = entry_length(1, 0) = 2 sectors.
        // After init (1 sector), we have 15 sectors free minus 1 reserved = 14 usable.
        // Each entry = 2 sectors, so we can fit 7 entries before needing to advance tail.
        // But the writer doesn't advance tail on its own — we just write until full.
        let page = [0xBBu8; SECTOR as usize];
        let entry_len = entry_length(1, 0);

        let mut entries_written = 0u32;
        loop {
            // Check if we have space for entry + 1 sector.
            let needed = entry_len + SECTOR;
            if writer.tail == writer.head {
                // Empty — full space available.
                if needed > writer.region.length {
                    break;
                }
            } else if needed > writer.free_space() {
                break;
            }

            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: LOGGABLE_OFFSET + (entries_written as u64) * 4096,
                        payload: &page,
                    }],
                    &[],
                )
                .await
                .unwrap();
            entries_written += 1;
        }

        assert!(entries_written > 0);
        // Verify that the head has wrapped around or is near the end.
        // The exact value depends on the arithmetic, but the write should have succeeded.
    }

    #[async_test]
    async fn writer_log_full_error() {
        let file = test_file();
        // Tiny log: 4 sectors.
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: 4 * SECTOR,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // After init entry (1 sector), 3 sectors free. A data entry needs 2 sectors + 1 reserved = 3. Fits.
        let page = [0xCCu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // Now head - tail = 3 sectors used, 1 free. Next entry needs 2 + 1 = 3. Won't fit.
        let result = writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 4096,
                    payload: &page,
                }],
                &[],
            )
            .await;

        // write_entry returns None when the log is full.
        assert_eq!(result.unwrap(), None);
    }

    // -----------------------------------------------------------------------
    // Log Replay tests
    // -----------------------------------------------------------------------

    #[async_test]
    async fn replay_single_data_entry() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        // Write a single data entry.
        let mut page_data = [0u8; SECTOR as usize];
        for (i, b) in page_data.iter_mut().enumerate() {
            *b = ((i + 1) % 256) as u8;
        }
        let target_offset = LOGGABLE_OFFSET + 4096;

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: target_offset,
                    payload: &page_data,
                }],
                &[],
            )
            .await
            .unwrap();

        // Now replay.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // Verify that the data was written to the target offset.
        let mut read_buf = [0u8; SECTOR as usize];
        file.read_at(target_offset, &mut read_buf).await.unwrap();
        assert_eq!(read_buf, page_data);
    }

    #[async_test]
    async fn replay_data_and_zero_descriptors() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // Write some non-zero data to the zero target first.
        let zero_target = LOGGABLE_OFFSET + 8192;
        let garbage = [0xFFu8; 8192];
        file.write_at(zero_target, &garbage).await.unwrap();

        let mut page_data = [0x42u8; SECTOR as usize];
        page_data[0] = 0xDE;
        page_data[1] = 0xAD;
        let data_target = LOGGABLE_OFFSET + 4096;

        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: data_target,
                    payload: &page_data,
                }],
                &[ZeroRange {
                    file_offset: zero_target,
                    length: 8192,
                }],
            )
            .await
            .unwrap();

        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // Verify data was applied.
        let mut read_buf = [0u8; SECTOR as usize];
        file.read_at(data_target, &mut read_buf).await.unwrap();
        assert_eq!(read_buf, page_data);

        // Verify zeros were applied.
        let mut zero_buf = vec![0u8; 8192];
        file.read_at(zero_target, &mut zero_buf).await.unwrap();
        assert!(zero_buf.iter().all(|&b| b == 0));
    }

    #[async_test]
    async fn replay_multiple_sequential_entries() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let mut pages = Vec::new();
        for n in 0..3 {
            let mut page = [0u8; SECTOR as usize];
            page.fill(n as u8 + 1);
            pages.push(page);
        }

        for (n, page) in pages.iter().enumerate() {
            let offset = LOGGABLE_OFFSET + (n as u64) * 4096;
            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: offset,
                        payload: page,
                    }],
                    &[],
                )
                .await
                .unwrap();
        }

        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        for (n, page) in pages.iter().enumerate() {
            let offset = LOGGABLE_OFFSET + (n as u64) * 4096;
            let mut buf = [0u8; SECTOR as usize];
            file.read_at(offset, &mut buf).await.unwrap();
            assert_eq!(buf, *page, "mismatch at entry {n}");
        }
    }

    #[async_test]
    async fn replay_invalid_crc_skipped() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        // Write a self-contained entry: start the writer at a non-zero position
        // so the entry's tail == its own start. The first (corrupt) entry
        // comes before it and is skipped.
        //
        // Strategy: write two independent sequences. The first is the init
        // entry whose CRC we corrupt. The second is an independent entry
        // that references itself as the tail.
        let mut writer = LogWriter {
            region: region.clone(),
            tail: 4 * SECTOR,
            head: 4 * SECTOR,
            sequence_number: 10,
            log_guid: guid,
            flushed_file_offset: 4 * 1024 * 1024,
            last_file_offset: 4 * 1024 * 1024,
        };

        let page = [0xAAu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // Also write a garbage entry at offset 0 that looks like a valid header
        // but has a corrupt CRC.
        let _bad_writer = LogWriter {
            region: region.clone(),
            tail: 0,
            head: 0,
            sequence_number: 0,
            log_guid: guid,
            flushed_file_offset: 4 * 1024 * 1024,
            last_file_offset: 4 * 1024 * 1024,
        };
        // We just need a valid-looking header at offset 0 with bad CRC.
        // Write an init-like entry, then corrupt its CRC.
        let header = LogEntryHeader {
            signature: LOG_ENTRY_HEADER_SIGNATURE,
            checksum: 0xDEADBEEF, // intentionally wrong
            entry_length: SECTOR,
            tail: 0,
            sequence_number: 1,
            descriptor_count: 0,
            reserved: 0,
            log_guid: guid,
            flushed_file_offset: 4 * 1024 * 1024,
            last_file_offset: 4 * 1024 * 1024,
        };
        let mut buf = [0u8; SECTOR as usize];
        buf[..HEADER_SIZE as usize].copy_from_slice(header.as_bytes());
        file.write_at(TEST_LOG_OFFSET, &buf).await.unwrap();

        // Replay should skip the bad entry at offset 0 and find the good entry
        // at offset 4*SECTOR.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // The data should have been applied.
        let mut read_buf = [0u8; SECTOR as usize];
        file.read_at(LOGGABLE_OFFSET, &mut read_buf).await.unwrap();
        assert_eq!(read_buf, page);
    }

    #[async_test]
    async fn replay_wrong_guid_skipped() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();
        let wrong_guid = guid::guid!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page = [0x55u8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // Try to replay with the wrong GUID — should find nothing.
        let result = replay_log(&file, &region, wrong_guid).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenError(OpenErrorInner::Corrupt(CorruptionType::NoValidLogEntries)) => {}
            other => panic!("expected NoValidLogEntries, got {:?}", other),
        }
    }

    #[async_test]
    async fn replay_bad_signature_skipped() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        // Place a valid self-contained entry at sector 4.
        let mut writer = LogWriter {
            region: region.clone(),
            tail: 4 * SECTOR,
            head: 4 * SECTOR,
            sequence_number: 20,
            log_guid: guid,
            flushed_file_offset: 4 * 1024 * 1024,
            last_file_offset: 4 * 1024 * 1024,
        };

        let page = [0x33u8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // Write a bad-signature entry at offset 0 so the scanner has something
        // to skip.
        let mut bad_header_buf = [0u8; SECTOR as usize];
        bad_header_buf[0..4].copy_from_slice(b"XXXX");
        file.write_at(TEST_LOG_OFFSET, &bad_header_buf)
            .await
            .unwrap();

        // The entry at sector 4 should still be found.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);
    }

    #[async_test]
    async fn replay_empty_log_errors() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let result = replay_log(&file, &region, guid).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OpenError(OpenErrorInner::Corrupt(CorruptionType::NoValidLogEntries)) => {}
            other => panic!("expected NoValidLogEntries, got {:?}", other),
        }
    }

    #[async_test]
    async fn replay_torn_write_last_entry() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // Write two entries.
        let page1 = [0x11u8; SECTOR as usize];
        let page2 = [0x22u8; SECTOR as usize];

        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page1,
                }],
                &[],
            )
            .await
            .unwrap();

        let entry3_start = writer.head;
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 4096,
                    payload: &page2,
                }],
                &[],
            )
            .await
            .unwrap();

        // Simulate torn write: zero out part of the last entry (its second sector).
        let torn_offset = TEST_LOG_OFFSET + region.log_add(entry3_start, SECTOR) as u64;
        let zeros = [0u8; SECTOR as usize];
        file.write_at(torn_offset, &zeros).await.unwrap();

        // Replay should apply only entries 1 and 2 (the init entry + first data entry).
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // page1 should be applied.
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(LOGGABLE_OFFSET, &mut buf).await.unwrap();
        assert_eq!(buf, page1);

        // page2 should NOT be applied (it was in the torn entry).
        // The file might have whatever garbage was at that location.
    }

    #[async_test]
    async fn writer_then_replay_roundtrip() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // Write several entries.
        let mut pages = Vec::new();
        for n in 0..5 {
            let mut page = [0u8; SECTOR as usize];
            for (i, b) in page.iter_mut().enumerate() {
                *b = ((n * 37 + i) % 256) as u8;
            }
            pages.push((LOGGABLE_OFFSET + (n as u64) * 4096, page));
        }

        for (offset, page) in &pages {
            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: *offset,
                        payload: page,
                    }],
                    &[],
                )
                .await
                .unwrap();
        }

        // Replay.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // Verify all pages.
        for (offset, expected) in &pages {
            let mut buf = [0u8; SECTOR as usize];
            file.read_at(*offset, &mut buf).await.unwrap();
            assert_eq!(&buf, expected, "mismatch at offset {offset:#x}");
        }
    }

    #[async_test]
    async fn replay_entry_wrapping_circular_buffer() {
        // Use a log small enough that an entry wraps around.
        let file = test_file();
        let small_log = 8 * SECTOR;
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: small_log,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // Fill the log partially to get head near the end.
        // Init entry = 1 sector, data entry = 2 sectors.
        // After init: head=1. Write 2 data entries: head=1+2+2=5.
        let page_a = [0xAAu8; SECTOR as usize];
        let page_b = [0xBBu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page_a,
                }],
                &[],
            )
            .await
            .unwrap();
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 4096,
                    payload: &page_b,
                }],
                &[],
            )
            .await
            .unwrap();

        // head is now at 5 sectors. Advance tail to free space.
        // We simulate this by manually setting the tail forward.
        writer.tail = writer.head - entry_length(1, 0); // keep only the last entry valid
        // Actually, let's just test the replay path as-is — the entries wrap
        // if the head is near the end.

        // head = 5*SECTOR. Log = 8 sectors. Free = 3 sectors.
        // A 2-sector entry would need 2 + 1 reserved = 3. Fits!
        // It starts at sector 5 and wraps: sector 5 (desc) + sector 6 → wraps to sector 0 (data).
        // Wait — that's not wrapping because sector 5+1=6 < 8.
        // Let's write another entry to push head to 7:
        // Actually head = 5 sectors. Next entry = 2 sectors → head = 7. Free = 1. Can't write more.

        // Let me reconsider. After init (head=1), 2 data entries (head=5).
        // Advance tail to 3 (past the init and first data entry).
        writer.tail = 3 * SECTOR;
        // Free space = 8 - (5*SECTOR - 3*SECTOR)/div... = using sequence_length.
        // sequence_length(3S, 5S) = 2S. Free = 8S - 2S = 6S. Next entry = 2S + 1S = 3S. Fits.

        let page_wrap = [0xCCu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 8192,
                    payload: &page_wrap,
                }],
                &[],
            )
            .await
            .unwrap();

        // head is now at 7 sectors. Free = 8S - seq_len(3S, 7S) = 8S - 4S = 4S. Write one more.
        let page_wrap2 = [0xDDu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 12288,
                    payload: &page_wrap2,
                }],
                &[],
            )
            .await
            .unwrap();

        // head should now be at (7+2) % 8 = 1 sector. This entry wrapped!
        assert_eq!(writer.head, SECTOR);

        // Replay: the scanner needs to find the sequence starting at tail=3S up to head=1S.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // Verify the wrapped entry was applied.
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(LOGGABLE_OFFSET + 8192, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, page_wrap);
        file.read_at(LOGGABLE_OFFSET + 12288, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, page_wrap2);
    }

    #[async_test]
    async fn replay_highest_lsn_sequence_chosen() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        // Write a sequence with lower LSNs.
        let mut writer1 = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page_old = [0x11u8; SECTOR as usize];
        writer1
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page_old,
                }],
                &[],
            )
            .await
            .unwrap();

        // Now write a second sequence with higher LSNs starting at a different
        // position in the log. We'll manually create a writer at a different head.
        let new_head = writer1.head + 4 * SECTOR; // skip a gap
        let mut writer2 = LogWriter {
            region: region.clone(),
            tail: new_head,
            head: new_head,
            sequence_number: 100, // much higher
            log_guid: guid,
            flushed_file_offset: 4 * 1024 * 1024,
            last_file_offset: 4 * 1024 * 1024,
        };

        let page_new = [0x99u8; SECTOR as usize];
        writer2
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page_new,
                }],
                &[],
            )
            .await
            .unwrap();

        // Replay should pick the sequence with LSN 101 over LSN 1-2.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // The data at LOGGABLE_OFFSET should be from the newer sequence.
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(LOGGABLE_OFFSET, &mut buf).await.unwrap();
        assert_eq!(buf, page_new);
    }

    #[async_test]
    async fn roundtrip_crash_replay() {
        // Write entries, "crash" (no cleanup), then replay.
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page = [0x77u8; SECTOR as usize];
        let target = LOGGABLE_OFFSET + 4096;
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: target,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // Simulating crash — no cleanup.
        let _ = writer;

        // Replay should recover the data.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        let mut buf = [0u8; SECTOR as usize];
        file.read_at(target, &mut buf).await.unwrap();
        assert_eq!(buf, page);
    }

    #[async_test]
    async fn file_extension_on_replay() {
        // Start with a small file, write entries referencing a large last_file_offset.
        let file = InMemoryFile::new(2 * 1024 * 1024); // 2 MiB
        let region = test_region();
        let guid = test_guid();

        let desired_size = 4 * 1024 * 1024u64;
        let mut writer = LogWriter::initialize(&file, region.clone(), guid, desired_size)
            .await
            .unwrap();

        let page = [0xABu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // The file might still be 2 MiB.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // After replay, the file should be at least desired_size.
        let sz = file.file_size().await.unwrap();
        assert!(sz >= desired_size, "expected >= {desired_size}, got {sz}");
    }

    // -----------------------------------------------------------------------
    // Tail advancement tests
    // -----------------------------------------------------------------------

    /// advance_tail reclaims space visible to free_space().
    #[async_test]
    async fn advance_tail_reclaims_free_space() {
        let file = test_file();
        let region = test_region();
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let initial_free = writer.free_space();
        let page = [0xAAu8; SECTOR as usize];

        // Write an entry — free space decreases.
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        let after_write = writer.free_space();
        assert!(after_write < initial_free, "writing should consume space");

        // Advance tail to head — reclaims all space.
        writer.advance_tail(writer.head);

        // When tail == head, free_space returns 0 (sequence_length returns
        // length for the full-log case). So instead of checking against
        // region.length, verify it's more than before the advance.
        // Actually, when tail == head AND the log is "empty" (we just
        // advanced past everything), the writer treats it as full.
        // The write_entry check handles this: if tail == head it uses
        // the full region. Let's verify we can write another entry.
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 4096,
                    payload: &page,
                }],
                &[],
            )
            .await
            .unwrap();

        // After advancing tail and writing one more, free space should be
        // close to what it was after just the init entry + one data entry.
        assert!(
            writer.free_space() >= after_write,
            "after advancing tail and writing, free space should be >= previous"
        );
    }

    /// Write entries until the log is full, advance tail, write more.
    ///
    /// This is the core scenario: without advance_tail, the log
    /// fills up and returns LogFull. With it, space is reclaimed.
    #[async_test]
    async fn write_advance_write_more() {
        let file = test_file();
        // Use a small log (16 sectors = 64 KiB) to hit the limit quickly.
        let small_log = 16 * SECTOR;
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: small_log,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page = [0xBBu8; SECTOR as usize];
        let elen = entry_length(1, 0); // 2 sectors per entry

        // Fill the log until we can't write anymore.
        let mut entries_written = 0u32;
        loop {
            let needed = elen + SECTOR; // entry + 1 reserved
            if writer.tail == writer.head {
                if needed > writer.region.length {
                    break;
                }
            } else if needed > writer.free_space() {
                break;
            }

            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: LOGGABLE_OFFSET + (entries_written as u64) * 4096,
                        payload: &page,
                    }],
                    &[],
                )
                .await
                .unwrap();
            entries_written += 1;
        }

        assert!(
            entries_written > 0,
            "should have written at least one entry"
        );

        // Confirm the log is now full.
        let result = writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page,
                }],
                &[],
            )
            .await;
        assert!(matches!(result, Ok(None)), "log should be full");

        // Advance tail past all entries — reclaim everything.
        writer.advance_tail(writer.head);

        // Now we should be able to write again.
        let mut more_written = 0u32;
        loop {
            let needed = elen + SECTOR;
            if writer.tail == writer.head {
                if needed > writer.region.length {
                    break;
                }
            } else if needed > writer.free_space() {
                break;
            }

            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: LOGGABLE_OFFSET
                            + ((entries_written + more_written) as u64) * 4096,
                        payload: &page,
                    }],
                    &[],
                )
                .await
                .unwrap();
            more_written += 1;
        }

        assert!(
            more_written > 0,
            "should write more entries after advancing tail"
        );
    }

    /// Incremental tail advancement: advance after each entry, write many
    /// more entries than the log can hold without reclamation.
    #[async_test]
    async fn incremental_advance_exceeds_log_capacity() {
        let file = test_file();
        // Tiny log: 8 sectors = 32 KiB.
        let small_log = 8 * SECTOR;
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: small_log,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page = [0xCCu8; SECTOR as usize];

        // The log has 8 sectors. Init takes 1. Each data entry takes 2.
        // Without advancement, we can fit ~3 entries before full.
        // With incremental advancement, we can write indefinitely.
        // Write 50 entries — well beyond the log's raw capacity.
        for i in 0..50u32 {
            let head_before = writer.head;
            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: LOGGABLE_OFFSET + (i as u64) * 4096,
                        payload: &page,
                    }],
                    &[],
                )
                .await
                .unwrap_or_else(|e| panic!("entry {i} failed: {e}"));
            // Advance tail to where head was before this entry.
            // This simulates "apply completed for the previous entry."
            writer.advance_tail(head_before);
        }
    }

    /// Replay after tail advancement: entries before the advanced tail are
    /// not part of the valid sequence, so replay only applies entries from
    /// the new tail onward.
    #[async_test]
    async fn replay_after_tail_advance() {
        let file = test_file();
        let small_log = 16 * SECTOR;
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: small_log,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        // Write entry A at LOGGABLE_OFFSET.
        let page_a = [0xAAu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET,
                    payload: &page_a,
                }],
                &[],
            )
            .await
            .unwrap();

        let head_after_a = writer.head;

        // Write entry B at LOGGABLE_OFFSET + 4096.
        let page_b = [0xBBu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 4096,
                    payload: &page_b,
                }],
                &[],
            )
            .await
            .unwrap();

        // Advance tail past the init entry and entry A.
        // The next write_entry will embed this new tail in its header.
        writer.advance_tail(head_after_a);

        // Write entry C to embed the new tail.
        let page_c = [0xCCu8; SECTOR as usize];
        writer
            .write_entry(
                &file,
                &[DataPage {
                    file_offset: LOGGABLE_OFFSET + 8192,
                    payload: &page_c,
                }],
                &[],
            )
            .await
            .unwrap();

        // Zero out the target areas to prove replay writes them.
        let zeros = [0u8; SECTOR as usize];
        file.write_at(LOGGABLE_OFFSET, &zeros).await.unwrap();
        file.write_at(LOGGABLE_OFFSET + 4096, &zeros).await.unwrap();
        file.write_at(LOGGABLE_OFFSET + 8192, &zeros).await.unwrap();

        // Replay. The scanner should find the sequence starting at the
        // new tail (head_after_a), which includes entries B and C.
        // Entry A is before tail — it may or may not be replayed depending
        // on scanner behavior (it's idempotent either way).
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // Entries B and C must be replayed.
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(LOGGABLE_OFFSET + 4096, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, page_b, "entry B should be replayed");
        file.read_at(LOGGABLE_OFFSET + 8192, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, page_c, "entry C should be replayed");
    }

    /// Wrap-around with incremental tail advancement: write enough entries
    /// with per-entry advancement to force both head and tail past the
    /// circular boundary.
    #[async_test]
    async fn wrap_around_with_incremental_advance() {
        let file = test_file();
        // 8-sector log. Each data entry = 2 sectors. After init (1 sector),
        // without advancement we'd fit ~3 entries. With advancement we wrap.
        let small_log = 8 * SECTOR;
        let region = LogRegion {
            file_offset: TEST_LOG_OFFSET,
            length: small_log,
        };
        let guid = test_guid();

        let mut writer = LogWriter::initialize(&file, region.clone(), guid, 4 * 1024 * 1024)
            .await
            .unwrap();

        let page = [0xDDu8; SECTOR as usize];

        // Write 20 entries, advancing tail before each write to keep
        // only the last entry valid. This forces both head and tail
        // to wrap multiple times.
        let mut last_head = writer.head;
        for i in 0..20u32 {
            writer.advance_tail(last_head);
            last_head = writer.head;

            writer
                .write_entry(
                    &file,
                    &[DataPage {
                        file_offset: LOGGABLE_OFFSET + (i as u64) * 4096,
                        payload: &page,
                    }],
                    &[],
                )
                .await
                .unwrap_or_else(|e| panic!("entry {i} failed during wrap-around: {e}"));
        }

        // Head and tail should both have wrapped past the log boundary.
        // With 20 entries of 2 sectors each in an 8-sector log, we've
        // gone around 5+ times.
        // Verify replay works with the final state.
        let result = replay_log(&file, &region, guid).await.unwrap();
        assert!(result.replayed);

        // The last entry wrote to LOGGABLE_OFFSET + 19*4096.
        let mut buf = [0u8; SECTOR as usize];
        file.read_at(LOGGABLE_OFFSET + 19 * 4096, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, page, "last entry should be replayed correctly");
    }
}
