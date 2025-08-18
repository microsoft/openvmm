// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A header + length-prefixed string buffer for storing logs over a buffer sized in 4K pages.
//!
//! Format (little-endian):
//! - Header
//!   - u16: total length in bytes of the data region currently used (sum of all encoded strings)
//!   - u32: number of messages that were dropped because the buffer was full
//! - Repeated entries:
//!   - u16: length of UTF-8 string payload. does not include the len of the u16 itself
//!   - [u8; len]: UTF-8 bytes
//!
//! Invariants:
//! - total_len <= DATA_CAPACITY (BUFFER_SIZE - HEADER_SIZE)
//! - Strings never partially written
//! - When append fails due to insufficient space, the dropped counter increments

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
    data_len: u16,
    next_insert: u16,
    dropped: u16,
}

#[derive(Debug, Error)]
enum EntryError {
    #[error("buffer too small for header")]
    BufferHeader,
    #[error("header len past remaining buffer")]
    BufferLen,
    #[error("buffer is not valid utf8")]
    BufferInvalidUtf8(#[source] Utf8Error),
}

/// an entry in the string buffer.
/// this consists of a u16 length, followed by a valid utf8 data payload.
#[derive(Debug, Clone, Copy)]
struct Entry<'a> {
    data: &'a str,
}

impl<'a> Entry<'a> {
    // parse an entry, and return remaining buffer
    fn parse(buf: &'a [u8]) -> Result<(Entry<'a>, &'a [u8]), EntryError> {
        let (header, rest) = buf.split_at_checked(2).ok_or(EntryError::BufferHeader)?;

        let len = u16::from_le_bytes(header.try_into().unwrap());
        let (data, rest) = rest
            .split_at_checked(len as usize)
            .ok_or(EntryError::BufferLen)?;

        let data = str::from_utf8(data).map_err(EntryError::BufferInvalidUtf8)?;

        Ok((Entry { data }, rest))
    }
}

/// A length-prefixed string buffer that stores strings in a 4K buffer.
///
/// The buffer format is a sequence of length-prefixed strings where each
/// string is prefixed by a u16 length value in little-endian format,
/// followed by the UTF-8 string data.
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
    #[error("string is too long to write to buffer")]
    StringTooLong,
    #[error("buffer is not 4k aligned")]
    BufferAlignment,
    #[error("buffer size is invalid")]
    BufferSize,
    #[error("header data len does not match buffer len")]
    InvalidHeaderDataLen,
    #[error("header next insert past end of buffer")]
    InvalidHeaderNextInsert,
    #[error("entry is invalid")]
    InvalidEntry(#[source] EntryError),
}

impl<'a> StringBuffer<'a> {
    fn validate_buffer(buffer: &[u8]) -> Result<(), StringBufferError> {
        // Buffer must be minimum of 4k or smaller than 15 pages, as the u16
        // used for next_insert cannot describe larger than that.
        if buffer.len() < PAGE_SIZE_4K || buffer.len() > PAGE_SIZE_4K * 15 {
            return Err(StringBufferError::BufferSize);
        }

        // Must be 4k aligned.
        if buffer.len() % PAGE_SIZE_4K != 0 {
            return Err(StringBufferError::BufferAlignment);
        }

        Ok(())
    }

    /// Creates a new empty string buffer from a 4K aligned buffer. The buffer
    /// must be between 4K or 60K.
    ///
    /// # Arguments
    /// * `buffer` - A mutable reference to a 4K aligned byte array for storage
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, StringBufferError> {
        Self::validate_buffer(buffer)?;

        let (header, data) = buffer.split_at_mut(size_of::<Header>());
        let header = Header::mut_from_bytes(header).expect("BUGBUG return error");
        header.data_len = data.len() as u16;
        header.next_insert = size_of::<Header>() as u16;
        header.dropped = 0;

        Ok(Self { header, data })
    }

    /// Creates a string buffer from an existing array that may contain data
    ///
    /// This function parses the existing buffer to verify the data is valid.
    ///
    /// # Arguments
    /// * `buffer` - A mutable reference to a 4K aligned byte array that may
    ///   contain existing data
    pub fn from_existing(buffer: &'a mut [u8]) -> Result<Self, StringBufferError> {
        Self::validate_buffer(buffer)?;

        let (header, data) = buffer.split_at_mut(size_of::<Header>());
        let header = Header::mut_from_bytes(header).expect("BUGBUG return error");

        // Validate header fields are valid
        if header.data_len as usize != data.len() {
            return Err(StringBufferError::InvalidHeaderDataLen);
        }

        let next_insert = header.next_insert as usize;
        if next_insert > data.len() {
            return Err(StringBufferError::InvalidHeaderNextInsert);
        }

        // Validate each individual entry is a valid entry
        let mut entries = data.split_at(next_insert).0;

        while entries.len() != 0 {
            let (_entry, rest) = Entry::parse(&entries).map_err(StringBufferError::InvalidEntry)?;
            entries = rest;
        }

        Ok(Self { header, data })
    }

    /// Appends a string to the buffer.
    ///
    /// The string will be stored with a length prefix (u16) followed by the
    /// UTF-8 data.
    ///
    /// # Arguments
    /// * `s` - The string to append
    ///
    /// # Returns
    /// `Ok(true)` if the string was successfully added. `Ok(false)` if the
    /// string is valid to add, but was dropped due to not enough space
    /// remaining.
    pub fn append(&mut self, s: &str) -> Result<bool, StringBufferError> {
        if s.len() > u16::MAX as usize {
            return Err(StringBufferError::StringTooLong);
        }

        let required_space = s.len() + 2;
        if required_space > self.remaining_capacity() {
            self.header.dropped = self.header.dropped.saturating_add(1);
            return Ok(false);
        }

        let (header, data) = &mut self.data
            [self.header.next_insert as usize..self.header.next_insert as usize + required_space]
            .split_at_mut(2);
        let str_len = s.len() as u16;
        header.copy_from_slice(str_len.as_bytes());
        data.copy_from_slice(s.as_bytes());

        self.header.next_insert += required_space as u16;

        Ok(true)
    }

    /// Returns an iterator over all strings in the buffer.
    ///
    /// # Returns
    /// An iterator that yields `Result<&str, StringBufferError>` for each string.
    pub fn iter(&self) -> StringBufferIterator<'_> {
        StringBufferIterator {
            entries: self.data.split_at(self.header.next_insert as usize).0,
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

/// Iterator over strings in a `StringBuffer`.
#[derive(Debug)]
pub struct StringBufferIterator<'a> {
    entries: &'a [u8],
}

impl<'a> Iterator for StringBufferIterator<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.entries.is_empty() {
            return None;
        }

        let (entry, rest) = Entry::parse(self.entries).expect("buffer should be valid");
        self.entries = rest;
        Some(entry.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::{vec, vec::Vec};

    #[test]
    fn test_new_buffer() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage).unwrap();
        assert_eq!(
            buffer.remaining_capacity(),
            DEFAULT_BUFFER_SIZE - HEADER_SIZE
        );
        assert_eq!(buffer.used_capacity(), HEADER_SIZE);
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped_messages(), 0);
        assert!(!buffer.is_full());
    }

    #[test]
    fn test_append_string() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let test_string = "Hello, World!";
        assert!(buffer.append(test_string).is_ok());
        let expected_used = HEADER_SIZE + 2 + test_string.len();
        assert_eq!(buffer.used_capacity(), expected_used);
        assert_eq!(
            buffer.remaining_capacity(),
            DEFAULT_BUFFER_SIZE - expected_used
        );
        assert!(!buffer.is_empty());
        assert!(!buffer.is_full());
    }

    #[test]
    fn test_append_multiple_strings() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        let strings = ["Hello", "World", "Test", "String"];
        for s in &strings {
            assert!(buffer.append(s).is_ok());
        }
        let collected: Result<Vec<&str>, _> = buffer.iter().collect();
        assert!(collected.is_ok());
        assert_eq!(collected.unwrap(), strings);
    }

    #[test]
    fn test_buffer_full() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        let data_capacity = storage.len() - HEADER_SIZE; // compute before mutable borrow in buffer
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        // Try to create a string that's larger than u16::MAX
        let large_string = "x".repeat(70000);
        let result = buffer.append(&large_string);
        assert_eq!(result, Err(StringBufferError::StringTooLong));
        // Fill buffer with maximum possible string (accounting for header & length prefix)
        let max_string = "x".repeat(data_capacity - 2);
        assert!(buffer.append(&max_string).is_ok());
        assert!(buffer.is_full());
        // Try to append another string (should drop)
        let result = buffer.append("test");
        assert_eq!(result, Err(StringBufferError::BufferFull));
        assert_eq!(buffer.dropped_messages(), 1);
    }

    #[test]
    fn test_from_existing_empty() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        // Header already zeroed -> empty
        let buffer = StringBuffer::from_existing(&mut storage).unwrap();
        assert_eq!(
            buffer.remaining_capacity(),
            DEFAULT_BUFFER_SIZE - HEADER_SIZE
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_from_existing_with_data() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        // Compose header + one string
        let s = "Hello";
        let len = s.len() as u16;
        // Write string entry after header
        let entry_offset = HEADER_SIZE;
        let len_bytes = len.to_le_bytes();
        storage[entry_offset] = len_bytes[0];
        storage[entry_offset + 1] = len_bytes[1];
        storage[entry_offset + 2..entry_offset + 2 + s.len()].copy_from_slice(s.as_bytes());
        // Header total_len
        let total_len_bytes = ((2 + s.len()) as u16).to_le_bytes();
        storage[0] = total_len_bytes[0];
        storage[1] = total_len_bytes[1];
        // dropped stays zero
        let buffer = StringBuffer::from_existing(&mut storage).unwrap();
        let strings: Result<Vec<&str>, _> = buffer.iter().collect();
        assert_eq!(strings.unwrap(), vec!["Hello"]);
    }

    #[test]
    fn test_clear() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage).unwrap();
        assert!(buffer.append("test").is_ok());
        assert!(!buffer.is_empty());
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(
            buffer.remaining_capacity(),
            DEFAULT_BUFFER_SIZE - HEADER_SIZE
        );
        assert_eq!(buffer.dropped_messages(), 0);
    }

    #[test]
    fn test_iterator_empty() {
        let mut storage = [0u8; DEFAULT_BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage).unwrap();
        let strings: Vec<_> = buffer.iter().collect();
        assert!(strings.is_empty());
    }
}
