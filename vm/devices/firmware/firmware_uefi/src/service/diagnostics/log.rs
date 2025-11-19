// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! This module provides the core `Log` type that represents a validated,
//! complete log entry with all necessary metadata. It consolidates parsing,
//! validation, and formatting logic in one place.

use std::borrow::Cow;
use std::mem::size_of;
use thiserror::Error;
use uefi_specs::hyperv::advanced_logger::AdvancedLoggerMessageEntryV2;
use uefi_specs::hyperv::advanced_logger::PHASE_NAMES;
use uefi_specs::hyperv::advanced_logger::SIG_ENTRY;
use uefi_specs::hyperv::debug_level::DEBUG_FLAG_NAMES;
use zerocopy::FromBytes;

/// 8-byte alignment requirement for every entry in the buffer
pub const ALIGNMENT: usize = 8;

/// Alignment mask for the entry
pub const ALIGNMENT_MASK: usize = ALIGNMENT - 1;

/// Maximum allowed size of a single message (4KB)
pub const MAX_MESSAGE_LENGTH: u16 = 0x1000;

/// Errors that occur when parsing log entries from untrusted buffer data
#[derive(Debug, Error)]
pub enum LogParseError {
    /// Entry signature does not match expected value
    #[error("Expected signature: {0:#x}, got: {1:#x}")]
    SignatureMismatch(u32, u32),
    /// Message length exceeds maximum allowed size
    #[error("Message length {1:#x} exceeds maximum {0:#x}")]
    MessageTooLong(u16, u16),
    /// Failed to read entry data from buffer slice
    #[error("Failed to read from buffer slice")]
    SliceRead,
    /// Arithmetic overflow occurred during calculation
    #[error("Arithmetic overflow in {0}")]
    Overflow(&'static str),
    /// Message contains invalid UTF-8 data
    #[error("Invalid UTF-8 in message: {0}")]
    Utf8Error(#[from] std::str::Utf8Error),
    /// Message end offset exceeds buffer bounds
    #[error("Message end {0:#x} exceeds buffer length {1:#x}")]
    OutOfBounds(usize, usize),
}

/// Represents a trusted log entry from the Advanced Logger buffer.
#[derive(Debug, Clone)]
pub struct Log {
    /// Debug level flags for this log entry
    pub debug_level: u32,
    /// Hypervisor reference ticks when the entry was created
    pub time_stamp: u64,
    /// Boot phase that produced this entry
    pub phase: u16,
    /// The log message content (validated UTF-8)
    pub message: String,
}

impl Log {
    /// Parse and validate a log entry from untrusted buffer data.
    ///
    /// # Arguments
    /// * `buffer` - The buffer slice to parse from
    ///
    /// # Returns
    /// A tuple of `(Log, bytes_consumed)` where `bytes_consumed` indicates how many
    /// bytes to advance in the buffer, or a `LogParseError` on failure.
    pub fn from_buffer(buffer: &[u8]) -> Result<(Self, usize), LogParseError> {
        let (raw_entry, _) = AdvancedLoggerMessageEntryV2::read_from_prefix(buffer)
            .map_err(|_| LogParseError::SliceRead)?;

        // Validate signature
        let expected_sig = u32::from_le_bytes(SIG_ENTRY);
        if raw_entry.signature != expected_sig {
            return Err(LogParseError::SignatureMismatch(
                expected_sig,
                raw_entry.signature,
            ));
        }

        // Validate message length
        if raw_entry.message_len > MAX_MESSAGE_LENGTH {
            return Err(LogParseError::MessageTooLong(
                MAX_MESSAGE_LENGTH,
                raw_entry.message_len,
            ));
        }

        // Extract and validate message
        let message_start = raw_entry.message_offset as usize;
        let message_end = message_start
            .checked_add(raw_entry.message_len as usize)
            .ok_or(LogParseError::Overflow("message_end"))?;

        if message_end > buffer.len() {
            return Err(LogParseError::OutOfBounds(message_end, buffer.len()));
        }

        let message = std::str::from_utf8(&buffer[message_start..message_end])?.to_string();

        // Calculate aligned entry size for buffer advancement
        let base_size = size_of::<AdvancedLoggerMessageEntryV2>()
            .checked_add(raw_entry.message_len as usize)
            .ok_or(LogParseError::Overflow("base_size"))?;

        let aligned_size = (base_size + ALIGNMENT_MASK) & !ALIGNMENT_MASK;

        Ok((
            Self {
                debug_level: raw_entry.debug_level,
                time_stamp: raw_entry.time_stamp,
                phase: raw_entry.phase,
                message,
            },
            aligned_size,
        ))
    }

    /// Check if this entry represents a complete message (ends with newline).
    pub fn is_complete(&self) -> bool {
        self.message.ends_with('\n')
    }

    /// Get the trimmed message (without trailing whitespace).
    pub fn message_trimmed(&self) -> &str {
        self.message.trim_end_matches(&['\r', '\n'][..])
    }

    /// Get the debug level as a human-readable string.
    pub fn debug_level_str(&self) -> Cow<'static, str> {
        debug_level_to_string(self.debug_level)
    }

    /// Get the phase as a human-readable string.
    pub fn phase_str(&self) -> &'static str {
        phase_to_string(self.phase)
    }

    /// Get the timestamp in ticks.
    pub fn ticks(&self) -> u64 {
        self.time_stamp
    }
}

/// Converts a debug level to a human-readable string
pub fn debug_level_to_string(debug_level: u32) -> Cow<'static, str> {
    // Borrow directly from the table if only one flag is set
    if debug_level.count_ones() == 1 {
        if let Some(&(_, name)) = DEBUG_FLAG_NAMES
            .iter()
            .find(|&&(flag, _)| flag == debug_level)
        {
            return Cow::Borrowed(name);
        }
    }

    // Handle combined flags or unknown debug levels
    let flags: Vec<&str> = DEBUG_FLAG_NAMES
        .iter()
        .filter(|&&(flag, _)| debug_level & flag != 0)
        .map(|&(_, name)| name)
        .collect();

    if flags.is_empty() {
        Cow::Borrowed("UNKNOWN")
    } else {
        Cow::Owned(flags.join("+"))
    }
}

/// Converts a phase value to a human-readable string
pub fn phase_to_string(phase: u16) -> &'static str {
    PHASE_NAMES
        .iter()
        .find(|&&(phase_raw, _)| phase_raw == phase)
        .map(|&(_, name)| name)
        .unwrap_or("UNKNOWN")
}
