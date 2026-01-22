// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A header + concatenated UTF-8 string buffer for storing logs backed by a 4K
//! aligned buffer.

#![no_std]
#![forbid(unsafe_code)]

use core::str;
use core::str::Utf8Error;
use thiserror::Error;
use zerocopy::FromBytes;
use zerocopy::Immutable;
use zerocopy::IntoBytes;
use zerocopy::KnownLayout;

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
///   - u16: total length in bytes of the data region (capacity usable for UTF-8
///     data)
///   - u16: next insertion offset (number of valid bytes currently used)
///   - u16: number of messages that were dropped because there was insufficient
///     space
/// - Data region: UTF-8 bytes
///
/// Invariants:
/// - next_insert <= data_len
/// - Data bytes [0, next_insert) always form valid UTF-8
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

        // Validate utf8 data is valid
        let used = &data[..next_insert];
        str::from_utf8(used).map_err(StringBufferError::InvalidUtf8)?;

        Ok(Self { header, data })
    }

    /// Appends a string to the buffer.
    ///
    /// The string is appended directly as UTF-8 bytes to the end of the current
    /// data payload (no delimiters).
    ///
    /// # Arguments
    /// * `s` - The string to append
    ///
    /// # Returns
    /// `Ok(true)` if the string was successfully added. `Ok(false)` if the
    /// string is valid to add, but was dropped due to not enough space
    /// remaining.
    pub fn append(&mut self, s: &str) -> Result<bool, StringBufferError> {
        if s.is_empty() {
            // Do not store empty strings.
            return Ok(true);
        }

        if s.len() > u16::MAX as usize {
            return Err(StringBufferError::StringTooLong);
        }

        let required_space = s.len();
        if required_space > self.remaining_capacity() {
            self.header.dropped = self.header.dropped.saturating_add(1);
            return Ok(false);
        }
        let start = self.header.next_insert as usize;
        let end = start + required_space;
        self.data[start..end].copy_from_slice(s.as_bytes());

        self.header.next_insert += required_space as u16;

        Ok(true)
    }

    /// Returns the concatenated UTF-8 contents stored so far.
    pub fn contents(&self) -> &str {
        str::from_utf8(&self.data[..self.header.next_insert as usize]).unwrap()
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

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::vec;
    use core::mem::size_of;

    const TEST_BUFFER_SIZE: usize = 4096; // 4K page

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
        assert_eq!(buffer.contents(), "");
    }

    #[test]
    fn test_append_string() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let test_string = "Hello, World!";
        assert!(buffer.append(test_string).is_ok());
        assert_eq!(buffer.contents(), test_string);
    }

    #[test]
    fn test_append_multiple_strings() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let strings = ["Hello", "World", "Test", "String"];
        for s in &strings {
            assert!(buffer.append(s).is_ok());
        }
        let expected = strings.join("");
        assert_eq!(buffer.contents(), expected);
    }

    #[test]
    fn test_buffer_full() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        // Try to create a string that's larger than u16::MAX
        let large_string = "x".repeat(70000);
        let result = buffer.append(&large_string);
        assert!(matches!(result, Err(StringBufferError::StringTooLong)));
        // Fill remaining capacity exactly
        let space = buffer.remaining_capacity();
        let max_string = "x".repeat(space);
        assert!(matches!(buffer.append(&max_string), Ok(true)));
        assert_eq!(buffer.remaining_capacity(), 0);
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
        assert_eq!(reopened.contents(), "");
    }

    #[test]
    fn test_from_existing_with_data() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        assert!(matches!(buffer.append("Hello"), Ok(true)));
        // Reconstruct using from_existing
        let buffer2 = StringBuffer::from_existing(&mut storage).unwrap();
        assert!(buffer2.contents().contains("Hello"));
    }

    #[test]
    fn test_contents_empty() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage).unwrap();
        assert_eq!(buffer.contents(), "");
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
        // next_insert = 1 (one byte used)
        storage[2..4].copy_from_slice(&1u16.to_le_bytes());
        // dropped = 0 (already zeroed)
        storage[header_size] = 0xFF; // invalid UTF-8
        let res = StringBuffer::from_existing(&mut storage);
        assert!(matches!(res, Err(StringBufferError::InvalidUtf8(_))));
    }

    #[test]
    fn test_append_multiple_drops_increment() {
        let mut storage = [0u8; TEST_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        // Fill the buffer completely
        let space = buffer.remaining_capacity();
        let filler = "x".repeat(space);
        assert!(matches!(buffer.append(&filler), Ok(true)));
        assert_eq!(buffer.remaining_capacity(), 0);
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
        let expected = strings.join("");
        assert_eq!(buffer.contents(), expected);
    }
}
