// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Entry parsing utilities for EFI diagnostics buffer

use std::mem::size_of;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerMessageEntryV2;
use uefi_specs::hyperv::advanced_logger::SIG_ENTRY;
use zerocopy::FromBytes;

/// 8-byte alignment for every entry
pub const ALIGNMENT: usize = 8;

/// Alignment mask for the entry
pub const ALIGNMENT_MASK: usize = ALIGNMENT - 1;

/// Maximum allowed size of a single message
pub const MAX_MESSAGE_LENGTH: u16 = 0x1000; // 4KB

/// Errors that occur when parsing entries
#[derive(Debug, Error)]
pub enum EntryParseError {
    /// Entry signature does not match expected value
    #[error("Expected: {0:#x}, got: {1:#x}")]
    SignatureMismatch(u32, u32),
    /// Message length exceeds maximum allowed size
    #[error("Expected message length < {0:#x}, got: {1:#x}")]
    MessageLength(u16, u16),
    /// Failed to read entry data from buffer slice
    #[error("Failed to read from buffer slice")]
    SliceRead,
    /// Arithmetic overflow occurred during calculation
    #[error("Arithmetic overflow in {0}")]
    Overflow(&'static str),
    /// Message contains invalid UTF-8 data
    #[error("Failed to read UTF-8 string: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),
    /// Message end offset exceeds buffer bounds
    #[error("message_end ({0:#x}) exceeds buffer slice length ({1:#x})")]
    BadMessageEnd(usize, usize),
}

/// Represents a single parsed entry from the EFI diagnostics buffer
#[derive(Debug)]
pub struct EntryData<'a> {
    /// The debug level of the log entry
    pub debug_level: u32,
    /// Timestamp of when the log entry was created
    pub time_stamp: u64,
    /// The boot phase that produced this log entry
    pub phase: u16,
    /// The log message itself
    pub message: &'a str,
    /// The size of the entry in bytes (including alignment)
    pub entry_size: usize,
}

/// Parse a single entry from a buffer slice
pub fn parse_entry(buffer_slice: &[u8]) -> Result<EntryData<'_>, EntryParseError> {
    let (entry, _) = AdvancedLoggerMessageEntryV2::read_from_prefix(buffer_slice)
        .map_err(|_| EntryParseError::SliceRead)?;

    let signature = entry.signature;
    if signature != u32::from_le_bytes(SIG_ENTRY) {
        return Err(EntryParseError::SignatureMismatch(
            u32::from_le_bytes(SIG_ENTRY),
            signature,
        ));
    }

    if entry.message_len > MAX_MESSAGE_LENGTH {
        return Err(EntryParseError::MessageLength(
            MAX_MESSAGE_LENGTH,
            entry.message_len,
        ));
    }

    let message_offset = entry.message_offset;
    let message_len = entry.message_len;

    // Calculate message start and end offsets for boundary validation
    let message_start = message_offset as usize;
    let message_end = message_start
        .checked_add(message_len as usize)
        .ok_or(EntryParseError::Overflow("message_end"))?;

    if message_end > buffer_slice.len() {
        return Err(EntryParseError::BadMessageEnd(
            message_end,
            buffer_slice.len(),
        ));
    }

    let message = std::str::from_utf8(&buffer_slice[message_start..message_end])?;

    // Calculate size of the entry to find the offset of the next entry
    let base_offset = size_of::<AdvancedLoggerMessageEntryV2>()
        .checked_add(message_len as usize)
        .ok_or(EntryParseError::Overflow("base_offset"))?;

    // Add padding for 8-byte alignment
    let aligned_offset = base_offset
        .checked_add(ALIGNMENT_MASK)
        .ok_or(EntryParseError::Overflow("aligned_offset"))?;
    let entry_size = aligned_offset & !ALIGNMENT_MASK;

    Ok(EntryData {
        debug_level: entry.debug_level,
        time_stamp: entry.time_stamp,
        phase: entry.phase,
        message,
        entry_size,
    })
}
