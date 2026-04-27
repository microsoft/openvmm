// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Dual header parsing, validation, and write-mode management for VHDX files.
//!
//! Reads both VHDX headers, validates their signatures and CRC-32C checksums,
//! selects the active header (higher sequence number), and validates log
//! region parameters.
//!
//! Also provides [`HeaderState`], which serializes all header writes behind
//! a `futures::lock::Mutex` and exposes the current [`WriteMode`] via an
//! `AtomicU8` for lock-free hot-path checks.

use crate::AsyncFile;
use crate::error::CorruptionType;
use crate::error::OpenError;
use crate::error::OpenErrorInner;
use crate::flush::FlushSequencer;
use crate::format;
use crate::format::Header;
use guid::Guid;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use zerocopy::FromBytes;
use zerocopy::FromZeros;
use zerocopy::IntoBytes;

/// Parsed and validated header data extracted from a VHDX file.
pub(crate) struct ParsedHeader {
    /// The active header's sequence number.
    pub sequence_number: u64,
    /// GUID changed on every file-level write.
    pub file_write_guid: Guid,
    /// GUID changed on every virtual-disk data write.
    pub data_write_guid: Guid,
    /// GUID identifying the active log. Zero means no active log.
    pub log_guid: Guid,
    /// File offset of the log region.
    pub log_offset: u64,
    /// Length of the log region in bytes.
    pub log_length: u32,
    /// True if header 1 was chosen as the active header.
    pub first_header_current: bool,
}

/// Read a single 4 KiB header from the file and validate its signature
/// and CRC-32C checksum. Returns `Some(header)` if valid, `None` otherwise.
async fn read_and_validate_header(
    file: &impl AsyncFile,
    offset: u64,
) -> Result<Option<Header>, OpenError> {
    let buf = file.alloc_buffer(format::HEADER_SIZE as usize);
    let buf = file
        .read_into(offset, buf)
        .await
        .map_err(OpenErrorInner::Io)?;

    // Check signature.
    let header = match Header::read_from_prefix(buf.as_ref()) {
        Ok((h, _)) => h,
        Err(_) => return Ok(None),
    };
    if header.signature != format::HEADER_SIGNATURE {
        return Ok(None);
    }

    // Validate CRC-32C checksum (checksum field is at byte offset 4).
    if !format::validate_checksum(buf.as_ref(), 4) {
        return Ok(None);
    }

    Ok(Some(header.clone()))
}

/// Read both headers from the file, validate them, and return the active one.
///
/// If both headers are valid, the one with the higher sequence number wins.
/// If only one is valid, it is used. If neither is valid, returns an error.
pub(crate) async fn parse_headers(
    file: &impl AsyncFile,
    file_length: u64,
) -> Result<ParsedHeader, OpenError> {
    let header1 = read_and_validate_header(file, format::HEADER_OFFSET_1).await?;
    let header2 = read_and_validate_header(file, format::HEADER_OFFSET_2).await?;

    // Choose the active header.
    let (header, first_header_current) = match (&header1, &header2) {
        (Some(h1), Some(h2)) => {
            if h1.sequence_number >= h2.sequence_number {
                (h1, true)
            } else {
                (h2, false)
            }
        }
        (Some(h1), None) => (h1, true),
        (None, Some(h2)) => (h2, false),
        (None, None) => return Err((CorruptionType::NoValidHeaders).into()),
    };

    // Validate version.
    if header.version != format::VERSION_1 {
        return Err((CorruptionType::UnsupportedVersion).into());
    }

    // If log GUID is non-zero, validate log version.
    if header.log_guid != Guid::ZERO && header.log_version != format::LOG_VERSION {
        return Err((CorruptionType::UnsupportedLogVersion).into());
    }

    // Validate log offset and length alignment.
    if !header.log_offset.is_multiple_of(format::REGION_ALIGNMENT)
        || !(header.log_length as u64).is_multiple_of(format::REGION_ALIGNMENT)
    {
        return Err((CorruptionType::InvalidLogOffsetOrLength).into());
    }

    let (log_offset, log_length) = if header.log_length == 0 {
        // Log is empty — log GUID must also be zero.
        if header.log_guid != Guid::ZERO {
            return Err((CorruptionType::MissingLogHasGuid).into());
        }
        (0, 0)
    } else {
        // Log is present — validate offset and bounds.
        if header.log_offset < format::HEADER_AREA_SIZE {
            return Err((CorruptionType::InvalidLogOffset).into());
        }
        if header.log_offset.saturating_add(header.log_length as u64) > file_length {
            return Err((CorruptionType::LogBeyondEndOfFile).into());
        }
        (header.log_offset, header.log_length)
    };

    Ok(ParsedHeader {
        sequence_number: header.sequence_number,
        file_write_guid: header.file_write_guid,
        data_write_guid: header.data_write_guid,
        log_guid: header.log_guid,
        log_offset,
        log_length,
        first_header_current,
    })
}

/// Serialize a VHDX header to a 4 KiB buffer with CRC and determine
/// the target offset (non-current header slot).
///
/// Returns `(buffer, file_offset)` ready for `write_from`.
pub(crate) fn serialize_header<F: AsyncFile>(
    file: &F,
    sequence_number: u64,
    file_write_guid: Guid,
    data_write_guid: Guid,
    log_guid: Guid,
    log_offset: u64,
    log_length: u32,
    first_header_current: bool,
) -> (F::Buffer, u64) {
    let mut header = Header::new_zeroed();
    header.signature = format::HEADER_SIGNATURE;
    header.sequence_number = sequence_number;
    header.file_write_guid = file_write_guid;
    header.data_write_guid = data_write_guid;
    header.log_guid = log_guid;
    header.log_version = format::LOG_VERSION;
    header.version = format::VERSION_1;
    header.log_length = log_length;
    header.log_offset = log_offset;
    header.checksum = 0;

    let mut buf = file.alloc_buffer(format::HEADER_SIZE as usize);
    let hdr_bytes = header.as_bytes();
    buf.as_mut()[..hdr_bytes.len()].copy_from_slice(hdr_bytes);
    let crc = format::compute_checksum(buf.as_ref(), 4);
    buf.as_mut()[4..8].copy_from_slice(&crc.to_le_bytes());

    let offset = if first_header_current {
        format::HEADER_OFFSET_2
    } else {
        format::HEADER_OFFSET_1
    };

    (buf, offset)
}

/// The kind of modification being made to the VHDX file. Controls which
/// GUIDs are updated in the header before the first write.
///
/// Values are ordered: `FileWritable < DataWritable`. Once `DataWritable`
/// is reached, `FileWritable` is a no-op. The `#[repr(u8)]` layout
/// matches the `AtomicU8` stored in [`HeaderState`].
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum WriteMode {
    /// The file is being modified (metadata only, e.g. resize/compact).
    /// Updates FileWriteGuid.
    FileWritable = 1,
    /// User-visible virtual disk data is being modified.
    /// Updates both FileWriteGuid and DataWriteGuid.
    DataWritable = 2,
}

/// Value used in [`HeaderState::write_mode`] when no write has occurred yet.
const WRITE_MODE_NONE: u8 = 0;

/// Mutable header state, serialized behind a `futures::lock::Mutex`.
///
/// All header writes go through [`HeaderState::write()`], which holds the
/// async mutex across the serialize→write→flush→flip sequence, preventing
/// concurrent header writes from interleaving.
///
/// The current [`WriteMode`] is also published to an `AtomicU8` so that
/// the hot path (`enable_write_mode`) can check it with a single atomic
/// load and avoid taking any lock.
pub(crate) struct HeaderState {
    /// Current write mode, published atomically for lock-free fast-path
    /// checks. Updated *after* the header is on stable storage.
    write_mode: AtomicU8,
    /// Data-write GUID, stored separately for the sync public accessor
    /// `VhdxFile::data_write_guid()`. Updated under the async mutex,
    /// read via `parking_lot::Mutex` (or `AtomicU64` pair if needed).
    /// Here we use `parking_lot::Mutex` since it's a brief, non-contended
    /// read.
    data_write_guid: parking_lot::Mutex<Guid>,
    /// File offset of the log region (immutable after open).
    log_offset: u64,
    /// Length of the log region in bytes (immutable after open).
    log_length: u32,
    /// Async mutex serializing all header writes.
    inner: futures::lock::Mutex<HeaderStateInner>,
}

/// Fields protected by the async mutex inside [`HeaderState`].
struct HeaderStateInner {
    /// Current header sequence number (bumped on every write).
    sequence_number: u64,
    /// GUID changed on every file-level write.
    file_write_guid: Guid,
    /// GUID changed on every virtual-disk data write.
    data_write_guid: Guid,
    /// Active log GUID. Zero when no log task is running.
    log_guid: Guid,
    /// True if header slot 1 (offset 64 KiB) is the current header.
    first_header_current: bool,
}

impl HeaderState {
    /// Create a new `HeaderState` from a parsed header.
    pub fn new(header: &ParsedHeader) -> Self {
        Self {
            write_mode: AtomicU8::new(WRITE_MODE_NONE),
            data_write_guid: parking_lot::Mutex::new(header.data_write_guid),
            log_offset: header.log_offset,
            log_length: header.log_length,
            inner: futures::lock::Mutex::new(HeaderStateInner {
                sequence_number: header.sequence_number,
                file_write_guid: header.file_write_guid,
                data_write_guid: header.data_write_guid,
                log_guid: header.log_guid,
                first_header_current: header.first_header_current,
            }),
        }
    }

    /// Lock-free check: is the current write mode ≥ `mode`?
    pub fn is_mode_enabled(&self, mode: WriteMode) -> bool {
        self.write_mode.load(Ordering::Acquire) >= mode as u8
    }

    /// Read the current data-write GUID (sync, brief lock).
    pub fn data_write_guid(&self) -> Guid {
        *self.data_write_guid.lock()
    }

    /// Get the log region offset and length (immutable after open).
    pub fn log_region(&self) -> (u64, u32) {
        (self.log_offset, self.log_length)
    }

    /// Read the current sequence number. Requires the async lock.
    #[cfg(test)]
    pub async fn sequence_number(&self) -> u64 {
        self.inner.lock().await.sequence_number
    }

    /// Read the current write mode (for test assertions).
    #[cfg(test)]
    pub fn write_mode(&self) -> Option<WriteMode> {
        match self.write_mode.load(Ordering::Acquire) {
            0 => None,
            1 => Some(WriteMode::FileWritable),
            2 => Some(WriteMode::DataWritable),
            _ => unreachable!(),
        }
    }

    /// Ensure the file is in at least write mode `mode`.
    ///
    /// Hot path (mode already enabled): single atomic load, no lock.
    ///
    /// Cold path (mode transition): acquires the async mutex, generates
    /// new GUIDs, writes the header to the non-current slot, flushes,
    /// flips the active slot, then publishes the new mode atomically.
    ///
    /// Safe to call concurrently — the async mutex serializes transitions.
    pub async fn enable_write_mode(
        &self,
        mode: WriteMode,
        file: &impl AsyncFile,
        flush_sequencer: Option<&FlushSequencer>,
    ) -> Result<(), std::io::Error> {
        // Hot path: single atomic load.
        if self.is_mode_enabled(mode) {
            return Ok(());
        }

        // Cold path: serialize under async mutex.
        let mut inner = self.inner.lock().await;

        // Double-check under lock (another caller may have raced).
        if self.write_mode.load(Ordering::Relaxed) >= mode as u8 {
            return Ok(());
        }

        // Generate new GUIDs.
        inner.file_write_guid = Guid::new_random();
        if mode >= WriteMode::DataWritable {
            inner.data_write_guid = Guid::new_random();
            *self.data_write_guid.lock() = inner.data_write_guid;
        }

        // Write header, flush, flip slot.
        self.write_header(&mut inner, file, flush_sequencer).await?;

        // Publish the mode change — only after the header is on stable storage.
        self.write_mode.store(mode as u8, Ordering::Release);

        Ok(())
    }

    /// Set the log GUID and write a header update. Used by `open_writable`
    /// to mark the file as dirty before spawning the log task.
    pub async fn set_log_guid(
        &self,
        log_guid: Guid,
        file: &impl AsyncFile,
        flush_sequencer: Option<&FlushSequencer>,
    ) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock().await;
        inner.log_guid = log_guid;
        self.write_header(&mut inner, file, flush_sequencer).await
    }

    /// Clear the log GUID (set to ZERO) and write a clean header.
    /// Used by `close()` after the log is fully drained.
    pub async fn clear_log_guid(
        &self,
        file: &impl AsyncFile,
        flush_sequencer: Option<&FlushSequencer>,
    ) -> Result<(), std::io::Error> {
        let mut inner = self.inner.lock().await;
        inner.log_guid = Guid::ZERO;
        self.write_header(&mut inner, file, flush_sequencer).await
    }

    /// Bump the sequence number, serialize the header, write to the
    /// non-current slot, flush, and flip the active slot.
    ///
    /// Caller must hold the async mutex (`inner` is `&mut`).
    async fn write_header(
        &self,
        inner: &mut HeaderStateInner,
        file: &impl AsyncFile,
        flush_sequencer: Option<&FlushSequencer>,
    ) -> Result<(), std::io::Error> {
        inner.sequence_number += 1;
        let (buf, offset) = serialize_header(
            file,
            inner.sequence_number,
            inner.file_write_guid,
            inner.data_write_guid,
            inner.log_guid,
            self.log_offset,
            self.log_length,
            inner.first_header_current,
        );

        file.write_from(offset, buf).await?;

        if let Some(fs) = flush_sequencer {
            fs.flush(file).await?;
        } else {
            file.flush().await?;
        }

        inner.first_header_current = !inner.first_header_current;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AsyncFileExt;
    use crate::tests::support::InMemoryFile;
    use pal_async::async_test;

    #[async_test]
    async fn parse_valid_dual_headers() {
        let (file, _params) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_length = file.file_size().await.unwrap();
        let parsed = parse_headers(&file, file_length).await.unwrap();

        // Header 2 has sequence_number 1. Header 1 has 0. So header 2 wins.
        assert_eq!(parsed.sequence_number, 1);
        assert!(!parsed.first_header_current);
        assert_eq!(parsed.log_guid, Guid::ZERO);
        assert_ne!(parsed.file_write_guid, Guid::ZERO);
        assert_ne!(parsed.data_write_guid, Guid::ZERO);
    }

    #[async_test]
    async fn parse_higher_sequence_wins() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_length = file.file_size().await.unwrap();

        // Corrupt header 1's CRC by flipping a byte.
        let mut buf = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_1, &mut buf)
            .await
            .unwrap();
        buf[10] ^= 0xFF;
        file.write_at(format::HEADER_OFFSET_1, &buf).await.unwrap();

        let parsed = parse_headers(&file, file_length).await.unwrap();
        // Header 1 is invalid, so header 2 is used.
        assert!(!parsed.first_header_current);
        assert_eq!(parsed.sequence_number, 1);
    }

    #[async_test]
    async fn parse_both_headers_corrupt() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_length = file.file_size().await.unwrap();

        // Corrupt both headers.
        let mut buf1 = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_1, &mut buf1)
            .await
            .unwrap();
        buf1[10] ^= 0xFF;
        file.write_at(format::HEADER_OFFSET_1, &buf1).await.unwrap();

        let mut buf2 = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_2, &mut buf2)
            .await
            .unwrap();
        buf2[10] ^= 0xFF;
        file.write_at(format::HEADER_OFFSET_2, &buf2).await.unwrap();

        let result = parse_headers(&file, file_length).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::NoValidHeaders
            )))
        ));
    }

    #[async_test]
    async fn parse_one_valid_header() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_length = file.file_size().await.unwrap();

        // Corrupt header 2's CRC.
        let mut buf = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_2, &mut buf)
            .await
            .unwrap();
        buf[10] ^= 0xFF;
        file.write_at(format::HEADER_OFFSET_2, &buf).await.unwrap();

        let parsed = parse_headers(&file, file_length).await.unwrap();
        assert!(parsed.first_header_current);
        assert_eq!(parsed.sequence_number, 0);
    }

    #[async_test]
    async fn parse_log_validation() {
        let (file, _) = InMemoryFile::create_test_vhdx(format::GB1).await;
        let file_length = file.file_size().await.unwrap();

        // Manually construct a header with valid signature but misaligned log.
        let mut buf = vec![0u8; format::HEADER_SIZE as usize];
        file.read_at(format::HEADER_OFFSET_1, &mut buf)
            .await
            .unwrap();

        let mut header = Header::read_from_prefix(&buf).unwrap().0.clone();
        header.log_offset = 12345; // Not aligned to REGION_ALIGNMENT.
        header.log_length = format::REGION_ALIGNMENT as u32;
        header.sequence_number = 100; // Make this the winning header.
        header.checksum = 0;

        // Write header bytes, recompute CRC.
        let header_bytes = IntoBytes::as_bytes(&header);
        buf[..header_bytes.len()].copy_from_slice(header_bytes);
        let crc = format::compute_checksum(&buf, 4);
        buf[4..8].copy_from_slice(&crc.to_le_bytes());
        file.write_at(format::HEADER_OFFSET_1, &buf).await.unwrap();

        let result = parse_headers(&file, file_length).await;
        assert!(matches!(
            result,
            Err(OpenError(OpenErrorInner::Corrupt(
                CorruptionType::InvalidLogOffsetOrLength
            )))
        ));
    }
}
