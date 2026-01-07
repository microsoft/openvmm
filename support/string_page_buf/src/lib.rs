// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A header + concatenated UTF-8 string buffer for storing logs backed by a 4K
//! aligned buffer.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;
use core::fmt::Write;
use core::str;
use core::str::Utf8Error;
use log::Level;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;
use zerocopy::Unaligned;
use zerocopy::little_endian::U16 as U16Le;

const PAGE_SIZE_4K: usize = 4096;

#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct Header {
    data_len: u16,    // capacity of the data region
    next_insert: u16, // number of bytes currently used (next offset)
    dropped: u16,     // number of dropped messages
}

/// A string buffer that stores UTF-8 data in a 4K aligned buffer. Note that the
/// header and data region are stored within the same buffer, as the header
/// precedes the data region.
///
/// Format:
/// - Header (6 bytes total)
///   - u16: total length in bytes of the data region (capacity usable for
///     entries)
///   - u16: next insertion offset (number of valid bytes currently used)
///   - u16: number of messages that were dropped because there was insufficient
///     space
/// - Data region: sequence of log entries, each consisting of:
///   - 1 byte: log level (0=Error, 1=Warn, 2=Info, 3=Debug, 4=Trace)
///   - 2 bytes: message length (u16 little-endian)
///   - N bytes: UTF-8 string data
///
/// Invariants:
/// - next_insert <= data_len
/// - Data bytes in string portions of entries always form valid UTF-8
/// - Appends never partially write data
/// - On insufficient space, the append is dropped and `dropped` is incremented
///
/// The in-memory representation stores:
/// - A reference to the 4K storage buffer
/// - The remaining capacity (calculated during initialization)
/// - The next byte offset for insertion
#[derive(Debug)]
pub struct StringBuffer<'a> {
    header: &'a mut Header,
    /// Reference to the rest of the data
    data: &'a mut [u8],
}

/// Header for each log entry in the buffer.
#[repr(C)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes, Unaligned)]
struct EntryHeader {
    /// Log level encoded as a byte (0=Error, 1=Warn, 2=Info, 3=Debug, 4=Trace)
    level: u8,
    /// Length of the message in bytes (little-endian, unaligned)
    msg_len: U16Le,
}

const ENTRY_HEADER_SIZE: usize = size_of::<EntryHeader>();

/// Converts a `log::Level` to a single byte for storage.
#[inline]
fn level_to_byte(level: Level) -> u8 {
    match level {
        Level::Error => 0,
        Level::Warn => 1,
        Level::Info => 2,
        Level::Debug => 3,
        Level::Trace => 4,
    }
}

/// Converts a stored byte back to a `log::Level`.
///
/// Returns `None` if the byte is not a valid log level.
#[inline]
pub fn byte_to_level(byte: u8) -> Option<Level> {
    match byte {
        0 => Some(Level::Error),
        1 => Some(Level::Warn),
        2 => Some(Level::Info),
        3 => Some(Level::Debug),
        4 => Some(Level::Trace),
        _ => None,
    }
}

/// A single log entry containing a level and message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogEntry<'a> {
    /// The log level of this entry.
    pub level: Level,
    /// The message content.
    pub message: &'a str,
}

/// Iterator over log entries in a StringBuffer.
///
/// Each entry consists of a log level and a message string.
pub struct LogEntryIter<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Iterator for LogEntryIter<'a> {
    type Item = LogEntry<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos + ENTRY_HEADER_SIZE > self.data.len() {
            return None;
        }

        let header_bytes = &self.data[self.pos..self.pos + ENTRY_HEADER_SIZE];
        let entry_header = EntryHeader::ref_from_bytes(header_bytes).ok()?;

        let level = byte_to_level(entry_header.level)?;
        let msg_len = entry_header.msg_len.get() as usize;

        let msg_start = self.pos + ENTRY_HEADER_SIZE;
        let msg_end = msg_start + msg_len;

        if msg_end > self.data.len() {
            return None;
        }

        let message_bytes = &self.data[msg_start..msg_end];
        let message = str::from_utf8(message_bytes).ok()?;
        self.pos = msg_end;

        Some(LogEntry { level, message })
    }
}

/// Error types that can occur when working with the string buffer.
#[derive(Debug, Error)]
pub enum StringBufferError {
    /// The string exceeds the maximum encodable u16 length.
    #[error("string is too long to write to buffer")]
    StringTooLong,
    /// The provided buffer is not u16 aligned to read the header.
    #[error("buffer is not u16 aligned")]
    BufferAlignment,
    /// The provided backing buffer length is not 4K aligned.
    #[error("buffer is not 4k aligned")]
    BufferSizeAlignment,
    /// The provided backing buffer size is outside the allowed range.
    #[error("buffer size is invalid")]
    BufferSize,
    /// The header's recorded data length does not match the actual data region length.
    #[error("header data len does not match buffer len")]
    InvalidHeaderDataLen,
    /// The header's next insertion offset is past the end of the data region.
    #[error("header next insert past end of buffer")]
    InvalidHeaderNextInsert,
    /// Existing used bytes are invalid UTF-8.
    #[error("buffer data is not valid utf8")]
    InvalidUtf8(#[source] Utf8Error),
}

impl<'a> StringBuffer<'a> {
    fn validate_buffer(buffer: &[u8]) -> Result<(), StringBufferError> {
        // Buffer must be minimum of 4k or smaller than 15 pages, as the u16
        // used for next_insert cannot describe larger than that.
        if buffer.len() < PAGE_SIZE_4K || buffer.len() > PAGE_SIZE_4K * 15 {
            return Err(StringBufferError::BufferSize);
        }

        // Must be 4k aligned.
        if !buffer.len().is_multiple_of(PAGE_SIZE_4K) {
            return Err(StringBufferError::BufferSizeAlignment);
        }

        Ok(())
    }

    /// Creates a new empty string buffer from a 4K aligned buffer. The buffer
    /// must be between 4K or 60K.
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, StringBufferError> {
        Self::validate_buffer(buffer)?;

        let (header, data) = buffer.split_at_mut(size_of::<Header>());
        let header =
            Header::mut_from_bytes(header).map_err(|_| StringBufferError::BufferAlignment)?;
        header.data_len = data.len() as u16;
        header.next_insert = 0;
        header.dropped = 0;

        Ok(Self { header, data })
    }

    /// Creates a string buffer from an existing buffer that may contain data.
    ///
    /// This function parses the existing buffer to verify the data is valid.
    pub fn from_existing(buffer: &'a mut [u8]) -> Result<Self, StringBufferError> {
        Self::validate_buffer(buffer)?;

        let (header, data) = buffer.split_at_mut(size_of::<Header>());
        let header =
            Header::mut_from_bytes(header).map_err(|_| StringBufferError::BufferAlignment)?;

        // Validate header fields are valid
        if header.data_len as usize != data.len() {
            return Err(StringBufferError::InvalidHeaderDataLen);
        }

        let next_insert = header.next_insert as usize;
        if next_insert > data.len() {
            return Err(StringBufferError::InvalidHeaderNextInsert);
        }

        // Validate entries are valid
        let used = &data[..next_insert];
        Self::validate_entries(used)?;

        Ok(Self { header, data })
    }

    /// Validates that the given data slice contains valid log entries.
    ///
    /// Each entry must have: level byte (0-4) + u16 length + valid UTF-8 data.
    fn validate_entries(data: &[u8]) -> Result<(), StringBufferError> {
        let mut pos = 0;
        while pos < data.len() {
            // Need at least ENTRY_HEADER_SIZE bytes for an entry header
            if pos + ENTRY_HEADER_SIZE > data.len() {
                return Err(StringBufferError::InvalidHeaderNextInsert);
            }

            // Read the entry header using zerocopy
            let header_bytes = &data[pos..pos + ENTRY_HEADER_SIZE];
            let entry_header = EntryHeader::ref_from_bytes(header_bytes)
                .map_err(|_| StringBufferError::InvalidHeaderNextInsert)?;

            // Validate level byte
            if byte_to_level(entry_header.level).is_none() {
                return Err(StringBufferError::InvalidHeaderNextInsert);
            }

            let msg_len = entry_header.msg_len.get() as usize;
            let msg_start = pos + ENTRY_HEADER_SIZE;
            let msg_end = msg_start + msg_len;

            if msg_end > data.len() {
                return Err(StringBufferError::InvalidHeaderNextInsert);
            }

            // Validate UTF-8
            let msg_bytes = &data[msg_start..msg_end];
            str::from_utf8(msg_bytes).map_err(StringBufferError::InvalidUtf8)?;

            pos = msg_end;
        }
        Ok(())
    }

    /// Appends a string to the buffer with the specified log level.
    ///
    /// The entry is stored as a 3-byte header (level + u16 length) followed by
    /// the UTF-8 string bytes.
    ///
    /// # Arguments
    /// * `level` - The log level for this entry
    /// * `s` - The string to append
    ///
    /// # Returns
    /// `Ok(true)` if the string was successfully added. `Ok(false)` if the
    /// string is valid to add, but was dropped due to not enough space
    /// remaining.
    fn append_with_level(&mut self, level: Level, s: &str) -> Result<bool, StringBufferError> {
        if s.is_empty() {
            // Do not store empty strings.
            return Ok(true);
        }

        if s.len() > u16::MAX as usize {
            return Err(StringBufferError::StringTooLong);
        }

        // Total length includes entry header (level + length) + string bytes
        let total_len = ENTRY_HEADER_SIZE + s.len();
        if total_len > self.remaining_capacity() {
            self.header.dropped = self.header.dropped.saturating_add(1);
            return Ok(false);
        }

        let start = self.header.next_insert as usize;

        let entry_header = EntryHeader {
            level: level_to_byte(level),
            msg_len: U16Le::new(s.len() as u16),
        };
        self.data[start..start + ENTRY_HEADER_SIZE].copy_from_slice(entry_header.as_bytes());

        // Write string data
        let str_start = start + ENTRY_HEADER_SIZE;
        let str_end = str_start + s.len();
        self.data[str_start..str_end].copy_from_slice(s.as_bytes());

        self.header.next_insert += total_len as u16;

        Ok(true)
    }

    /// Appends a string to the buffer with `Info` log level.
    ///
    /// The entry is stored as a 3-byte header (Info level + u16 length)
    /// followed by the UTF-8 string bytes.
    ///
    /// # Arguments
    /// * `s` - The string to append
    ///
    /// # Returns
    /// `Ok(true)` if the string was successfully added. `Ok(false)` if the
    /// string is valid to add, but was dropped due to not enough space
    /// remaining.
    pub fn append(&mut self, s: &str) -> Result<bool, StringBufferError> {
        self.append_with_level(Level::Info, s)
    }

    /// Appends a formatted log message to the buffer.
    ///
    /// This method accepts `fmt::Arguments` directly, allowing efficient
    /// formatting without intermediate allocations. The entry is stored as a
    /// 3-byte header (level + u16 length) followed by the formatted UTF-8
    /// string bytes.
    ///
    /// # Arguments
    /// * `level` - The log level for this entry
    /// * `args` - The format arguments to write
    ///
    /// # Returns
    /// `Ok(true)` if the message was successfully added. `Ok(false)` if the
    /// message was dropped due to not enough space remaining.
    pub fn append_log(
        &mut self,
        level: Level,
        args: &fmt::Arguments<'_>,
    ) -> Result<bool, StringBufferError> {
        // Check if we have at least space for the entry header
        if self.remaining_capacity() < ENTRY_HEADER_SIZE {
            self.header.dropped = self.header.dropped.saturating_add(1);
            return Ok(false);
        }

        let start = self.header.next_insert as usize;

        // Reserve space for header, write directly after it
        let str_start = start + ENTRY_HEADER_SIZE;
        let mut writer = SliceWriter {
            buf: &mut self.data[str_start..],
            pos: 0,
        };

        // Format directly into the buffer
        if write!(writer, "{}", args).is_err() {
            // Not enough space - don't update next_insert, increment dropped
            self.header.dropped = self.header.dropped.saturating_add(1);
            return Ok(false);
        }

        let bytes_written = writer.pos;
        if bytes_written == 0 {
            // Empty message - don't store anything
            return Ok(true);
        }

        if bytes_written > u16::MAX as usize {
            self.header.dropped = self.header.dropped.saturating_add(1);
            return Err(StringBufferError::StringTooLong);
        }

        // Message written, now write the header
        let entry_header = EntryHeader {
            level: level_to_byte(level),
            msg_len: U16Le::new(bytes_written as u16),
        };
        self.data[start..start + ENTRY_HEADER_SIZE].copy_from_slice(entry_header.as_bytes());

        let total_len = ENTRY_HEADER_SIZE + bytes_written;
        self.header.next_insert += total_len as u16;

        Ok(true)
    }

    /// Returns an iterator over log entries in the buffer.
    ///
    /// Each entry contains a log level and a message string.
    pub fn entries(&self) -> LogEntryIter<'_> {
        LogEntryIter {
            data: &self.data[..self.header.next_insert as usize],
            pos: 0,
        }
    }

    /// Returns the number of bytes remaining in the buffer.
    fn remaining_capacity(&self) -> usize {
        (self.header.data_len - self.header.next_insert) as usize
    }

    /// Returns number of dropped messages recorded in the header.
    pub fn dropped_messages(&self) -> u16 {
        self.header.dropped
    }
}

/// A writer that writes directly to a byte slice without allocation.
///
/// This is used internally to format `fmt::Arguments` directly into the buffer.
struct SliceWriter<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl Write for SliceWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buf.len() - self.pos;
        if bytes.len() > remaining {
            return Err(fmt::Error);
        }
        self.buf[self.pos..self.pos + bytes.len()].copy_from_slice(bytes);
        self.pos += bytes.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::mem::size_of;

    const TEST_BUFFER_SIZE: usize = 4096; // 4K page

    /// Helper to collect all entry messages into a Vec
    fn collect_messages<'a>(buffer: &'a StringBuffer<'_>) -> Vec<&'a str> {
        buffer.entries().map(|e| e.message).collect()
    }

    /// Helper to collect all entries
    fn collect_entries<'a>(buffer: &'a StringBuffer<'_>) -> Vec<LogEntry<'a>> {
        buffer.entries().collect()
    }

    #[test]
    fn test_new_buffer() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage).unwrap();
        let header_size = size_of::<Header>();
        // next_insert starts at header_size inside data region.
        // data_len == data capacity (storage - header_size)
        let expected_remaining = TEST_BUFFER_SIZE - header_size;
        assert_eq!(buffer.remaining_capacity(), expected_remaining);
        assert_eq!(buffer.dropped_messages(), 0);
        assert_eq!(collect_messages(&buffer).len(), 0);
    }

    #[test]
    fn test_append_string() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let test_string = "Hello, World!";
        assert!(buffer.append(test_string).is_ok());
        let entries = collect_entries(&buffer);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].message, test_string);
        assert_eq!(entries[0].level, Level::Info);
    }

    #[test]
    fn test_append_multiple_strings() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let strings = ["Hello", "World", "Test", "String"];
        for s in &strings {
            assert!(buffer.append(s).is_ok());
        }
        let messages = collect_messages(&buffer);
        assert_eq!(messages, strings);
    }

    #[test]
    fn test_buffer_full() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        // Try to create a string that's larger than u16::MAX
        let large_string = "x".repeat(70000);
        let result = buffer.append(&large_string);
        assert!(matches!(result, Err(StringBufferError::StringTooLong)));
        // Fill remaining capacity (accounting for entry header)
        let space = buffer.remaining_capacity();
        // Each entry needs ENTRY_HEADER_SIZE (3) bytes for the header
        let max_string = "x".repeat(space - ENTRY_HEADER_SIZE);
        assert!(matches!(buffer.append(&max_string), Ok(true)));
        // Now there's only ENTRY_HEADER_SIZE-1 bytes left, not enough for any entry
        assert!(buffer.remaining_capacity() < ENTRY_HEADER_SIZE + 1);
        // Try to append another string (should be dropped, Ok(false))
        let result = buffer.append("test");
        assert!(matches!(result, Ok(false)));
        assert_eq!(buffer.dropped_messages(), 1);
    }

    #[test]
    fn test_from_existing_empty() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        {
            // initialize header properly
            let _buf = StringBuffer::new(&mut storage).unwrap();
        }
        let reopened = StringBuffer::from_existing(&mut storage).unwrap();
        assert_eq!(collect_messages(&reopened).len(), 0);
    }

    #[test]
    fn test_from_existing_with_data() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        assert!(matches!(buffer.append("Hello"), Ok(true)));
        // Reconstruct using from_existing
        let buffer2 = StringBuffer::from_existing(&mut storage).unwrap();
        let messages = collect_messages(&buffer2);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0], "Hello");
    }

    #[test]
    fn test_entries_empty() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage).unwrap();
        assert_eq!(collect_messages(&buffer).len(), 0);
    }

    #[test]
    fn test_new_buffer_too_small() {
        let mut storage = [0u8; 1024];
        let res = StringBuffer::new(&mut storage);
        assert!(matches!(res, Err(StringBufferError::BufferSize)));
    }

    #[test]
    fn test_new_buffer_too_large() {
        // 16 pages (> 15 allowed)
        let mut storage = [0u8; PAGE_SIZE_4K * 16];
        let res = StringBuffer::new(&mut storage);
        assert!(matches!(res, Err(StringBufferError::BufferSize)));
    }

    #[test]
    fn test_new_buffer_misaligned() {
        // size not a multiple of 4K but within range
        let mut storage = vec![0u8; PAGE_SIZE_4K * 2 + 1];
        let res = StringBuffer::new(&mut storage);
        assert!(matches!(res, Err(StringBufferError::BufferSizeAlignment)));
    }

    #[test]
    fn test_from_existing_invalid_header_data_len() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let header_size = size_of::<Header>();
        let data_len = (TEST_BUFFER_SIZE - header_size) as u16;
        // Corrupt: set data_len to wrong value (0)
        storage[0..2].copy_from_slice(&0u16.to_le_bytes());
        // next_insert = 0, dropped = 0 already
        let res = StringBuffer::from_existing(&mut storage);
        assert!(matches!(res, Err(StringBufferError::InvalidHeaderDataLen)));
        // Make a valid header first then corrupt after creation
        storage[0..2].copy_from_slice(&data_len.to_le_bytes());
        // Now make next_insert invalid (past end)
        storage[2..4].copy_from_slice(&(data_len + 1).to_le_bytes());
        let res2 = StringBuffer::from_existing(&mut storage);
        assert!(matches!(
            res2,
            Err(StringBufferError::InvalidHeaderNextInsert)
        ));
    }

    #[test]
    fn test_from_existing_invalid_utf8() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let header_size = size_of::<Header>();
        let data_len = (TEST_BUFFER_SIZE - header_size) as u16;
        storage[0..2].copy_from_slice(&data_len.to_le_bytes());
        // Create a valid entry header: level=Info(2), length=1
        storage[header_size] = 2; // Info level
        storage[header_size + 1] = 1; // length low byte
        storage[header_size + 2] = 0; // length high byte
        // next_insert = 4 (header + 1 byte message)
        storage[2..4].copy_from_slice(&4u16.to_le_bytes());
        // dropped = 0 (already zeroed)
        storage[header_size + 3] = 0xFF; // invalid UTF-8 message byte
        let res = StringBuffer::from_existing(&mut storage);
        assert!(matches!(res, Err(StringBufferError::InvalidUtf8(_))));
    }

    #[test]
    fn test_append_multiple_drops_increment() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        // Fill the buffer completely (accounting for entry header)
        let space = buffer.remaining_capacity();
        let filler = "x".repeat(space - ENTRY_HEADER_SIZE);
        assert!(matches!(buffer.append(&filler), Ok(true)));
        // Buffer is now full (only has 0 bytes left, not enough for any entry)
        assert!(buffer.remaining_capacity() < ENTRY_HEADER_SIZE + 1);
        // Multiple failed appends increment dropped each time
        assert!(matches!(buffer.append("a"), Ok(false)));
        assert_eq!(buffer.dropped_messages(), 1);
        assert!(matches!(buffer.append("b"), Ok(false)));
        assert_eq!(buffer.dropped_messages(), 2);
        assert!(matches!(buffer.append("c"), Ok(false)));
        assert_eq!(buffer.dropped_messages(), 3);
    }

    #[test]
    fn test_append_utf8_strings() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let strings = ["hé", "über", "数据", "emoji 😊"];
        for s in &strings {
            assert!(matches!(buffer.append(s), Ok(true)));
        }
        let messages = collect_messages(&buffer);
        assert_eq!(messages, strings);
    }

    #[test]
    fn test_append_log_with_level() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();

        // Test append_log with different levels
        assert!(
            buffer
                .append_log(Level::Error, &format_args!("error msg"))
                .is_ok()
        );
        assert!(
            buffer
                .append_log(Level::Warn, &format_args!("warn msg"))
                .is_ok()
        );
        assert!(
            buffer
                .append_log(Level::Info, &format_args!("info msg"))
                .is_ok()
        );
        assert!(
            buffer
                .append_log(Level::Debug, &format_args!("debug msg"))
                .is_ok()
        );
        assert!(
            buffer
                .append_log(Level::Trace, &format_args!("trace msg"))
                .is_ok()
        );

        let entries = collect_entries(&buffer);
        assert_eq!(entries.len(), 5);
        assert_eq!(
            entries[0],
            LogEntry {
                level: Level::Error,
                message: "error msg"
            }
        );
        assert_eq!(
            entries[1],
            LogEntry {
                level: Level::Warn,
                message: "warn msg"
            }
        );
        assert_eq!(
            entries[2],
            LogEntry {
                level: Level::Info,
                message: "info msg"
            }
        );
        assert_eq!(
            entries[3],
            LogEntry {
                level: Level::Debug,
                message: "debug msg"
            }
        );
        assert_eq!(
            entries[4],
            LogEntry {
                level: Level::Trace,
                message: "trace msg"
            }
        );
    }

    #[test]
    fn test_append_log_formatted() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();

        let value = 42;
        let name = "test";
        assert!(
            buffer
                .append_log(Level::Info, &format_args!("value={}, name={}", value, name))
                .is_ok()
        );

        let entries = collect_entries(&buffer);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, Level::Info);
        assert_eq!(entries[0].message, "value=42, name=test");
    }

    #[test]
    fn test_append_log_empty() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();

        // Empty format should succeed but not store anything
        assert!(buffer.append_log(Level::Info, &format_args!("")).is_ok());
        assert_eq!(collect_entries(&buffer).len(), 0);
    }

    #[test]
    fn test_raw_append_uses_info_level() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();

        assert!(buffer.append("raw string").is_ok());

        let entries = collect_entries(&buffer);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, Level::Info);
        assert_eq!(entries[0].message, "raw string");
    }

    #[test]
    fn test_level_byte_conversion() {
        assert_eq!(level_to_byte(Level::Error), 0);
        assert_eq!(level_to_byte(Level::Warn), 1);
        assert_eq!(level_to_byte(Level::Info), 2);
        assert_eq!(level_to_byte(Level::Debug), 3);
        assert_eq!(level_to_byte(Level::Trace), 4);

        assert_eq!(byte_to_level(0), Some(Level::Error));
        assert_eq!(byte_to_level(1), Some(Level::Warn));
        assert_eq!(byte_to_level(2), Some(Level::Info));
        assert_eq!(byte_to_level(3), Some(Level::Debug));
        assert_eq!(byte_to_level(4), Some(Level::Trace));
        assert_eq!(byte_to_level(5), None);
        assert_eq!(byte_to_level(255), None);
    }

    #[test]
    fn test_validate_entries_empty() {
        // Empty data is valid
        assert!(StringBuffer::validate_entries(&[]).is_ok());
    }

    #[test]
    fn test_validate_entries_truncated_header() {
        // Only 1 byte - not enough for entry header (needs 3)
        assert!(StringBuffer::validate_entries(&[2]).is_err());
        // Only 2 bytes - still not enough
        assert!(StringBuffer::validate_entries(&[2, 0]).is_err());
    }

    #[test]
    fn test_validate_entries_invalid_level() {
        // Level byte = 5 is invalid (valid range is 0-4)
        let data = [5, 0, 0]; // level=5, msg_len=0
        assert!(StringBuffer::validate_entries(&data).is_err());

        // Level byte = 255 is invalid
        let data = [255, 0, 0]; // level=255, msg_len=0
        assert!(StringBuffer::validate_entries(&data).is_err());
    }

    #[test]
    fn test_validate_entries_msg_len_overflow() {
        // Valid header but msg_len points past end of data
        let data = [2, 10, 0]; // level=Info, msg_len=10, but no message bytes
        assert!(StringBuffer::validate_entries(&data).is_err());
    }

    #[test]
    fn test_validate_entries_invalid_utf8() {
        // Valid header but invalid UTF-8 in message
        let data = [2, 1, 0, 0xFF]; // level=Info, msg_len=1, message=0xFF (invalid UTF-8)
        assert!(matches!(
            StringBuffer::validate_entries(&data),
            Err(StringBufferError::InvalidUtf8(_))
        ));
    }

    #[test]
    fn test_validate_entries_single_valid() {
        // Valid entry: level=Info, msg_len=5, message="hello"
        let data = [2, 5, 0, b'h', b'e', b'l', b'l', b'o'];
        assert!(StringBuffer::validate_entries(&data).is_ok());
    }

    #[test]
    fn test_validate_entries_multiple_valid() {
        // Two valid entries
        let mut data = vec![];
        // Entry 1: level=Error, msg_len=2, message="hi"
        data.extend_from_slice(&[0, 2, 0, b'h', b'i']);
        // Entry 2: level=Warn, msg_len=3, message="bye"
        data.extend_from_slice(&[1, 3, 0, b'b', b'y', b'e']);
        assert!(StringBuffer::validate_entries(&data).is_ok());
    }

    #[test]
    fn test_validate_entries_second_entry_invalid() {
        // First entry valid, second entry has invalid level
        let mut data = vec![];
        // Entry 1: level=Info, msg_len=2, message="hi"
        data.extend_from_slice(&[2, 2, 0, b'h', b'i']);
        // Entry 2: level=99 (invalid), msg_len=0
        data.extend_from_slice(&[99, 0, 0]);
        assert!(StringBuffer::validate_entries(&data).is_err());
    }

    #[test]
    fn test_validate_entries_zero_length_message() {
        // Valid entry with zero-length message
        let data = [2, 0, 0]; // level=Info, msg_len=0
        assert!(StringBuffer::validate_entries(&data).is_ok());
    }
}
