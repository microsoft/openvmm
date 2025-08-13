// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A header + length-prefixed string buffer for storing logs over a buffer sized in 4K pages.
//!
//! Format (little-endian):
//! - Header
//!   - u16: total length in bytes of the data region currently used (sum of all encoded strings)
//!   - u32: number of messages that were dropped because the buffer was full
//! - Repeated entries:
//!   - u16: length of UTF-8 string
//!   - [u8; len]: UTF-8 bytes
//!
//! Invariants:
//! - total_len <= DATA_CAPACITY (BUFFER_SIZE - HEADER_SIZE)
//! - Strings never partially written
//! - When append fails due to insufficient space, the dropped counter increments
//!
//! NOTE: This is step 1 of the refactor: introduce header format while still
//! performing manual parsing. Step 2 will migrate parsing to `zerocopy` and
//! step 3 will replace the custom error with `thiserror`.

#![no_std]
#![forbid(unsafe_code)]

use core::str;
use zerocopy::{FromBytes, IntoBytes, Immutable, KnownLayout, LittleEndian, U16, U32};

/// Default size for a single page (4KB) buffer.
pub const DEFAULT_BUFFER_SIZE: usize = 4096;

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
    /// Reference to the storage buffer (must be multiple of 4K)
    buffer: &'a mut [u8],
    /// The next byte offset (absolute within buffer) where the next string should be inserted
    next_offset: usize,
    /// Remaining capacity in bytes (data region only + header unused space)
    remaining_capacity: usize,
}

/// Error types that can occur when working with the string buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringBufferError {
    /// The string is too long to fit in the buffer
    StringTooLong,
    /// The buffer is full and cannot accommodate more strings
    BufferFull,
    /// Invalid data format in the buffer
    InvalidFormat,
    /// String contains invalid UTF-8 data
    InvalidUtf8,
}

impl core::fmt::Display for StringBufferError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StringBufferError::StringTooLong => write!(f, "String is too long to fit in buffer"),
            StringBufferError::BufferFull => write!(f, "Buffer is full"),
            StringBufferError::InvalidFormat => write!(f, "Invalid buffer format"),
            StringBufferError::InvalidUtf8 => write!(f, "Invalid UTF-8 data"),
        }
    }
}

impl<'a> StringBuffer<'a> {
    /// Creates a new empty string buffer from a 4K storage array.
    ///
    /// # Arguments
    /// * `buffer` - A mutable reference to a 4K byte array for storage
    ///
    /// # Returns
    /// A new `StringBuffer` instance ready for use.
    pub fn new(buffer: &'a mut [u8]) -> Result<Self, StringBufferError> {
        if buffer.len() < HEADER_SIZE || buffer.len() % PAGE_SIZE != 0 {
            return Err(StringBufferError::InvalidFormat);
        }
        if buffer.len() - HEADER_SIZE > u16::MAX as usize {
            return Err(StringBufferError::InvalidFormat);
        }
        buffer.fill(0);
        Self::write_total_len(buffer, 0);
        Self::write_dropped(buffer, 0);
        let next_offset = HEADER_SIZE;
        let remaining_capacity = buffer.len() - HEADER_SIZE;
        Ok(Self {
            buffer,
            next_offset,
            remaining_capacity,
        })
    }

    /// Convenience helper for 4K fixed-size arrays.
    pub fn new_fixed(buffer: &'a mut [u8; DEFAULT_BUFFER_SIZE]) -> Self {
        Self::new(&mut buffer[..]).expect("valid 4K buffer")
    }

    /// Creates a string buffer from an existing 4K storage array that may contain data.
    ///
    /// This function parses the existing buffer to determine the current state,
    /// including the next insertion offset and remaining capacity.
    ///
    /// # Arguments
    /// * `buffer` - A mutable reference to a 4K byte array that may contain existing data
    ///
    /// # Returns
    /// A `StringBuffer` instance if the buffer format is valid, or an error.
    pub fn from_existing(buffer: &'a mut [u8]) -> Result<Self, StringBufferError> {
        if buffer.len() < HEADER_SIZE || buffer.len() % PAGE_SIZE != 0 {
            return Err(StringBufferError::InvalidFormat);
        }
        if buffer.len() - HEADER_SIZE > u16::MAX as usize {
            return Err(StringBufferError::InvalidFormat);
        }
        // Read header
        let total_len = Self::read_total_len(buffer) as usize;
        let data_capacity = buffer.len() - HEADER_SIZE;
        if total_len > data_capacity {
            return Err(StringBufferError::InvalidFormat);
        }

        // Validate entries using Entry parser
        let mut cursor = HEADER_SIZE;
        let data_end = HEADER_SIZE + total_len;
        if data_end > buffer.len() { return Err(StringBufferError::InvalidFormat); }
        while cursor < data_end {
            match Entry::parse(&buffer[cursor..data_end]) {
                Ok((entry, adv)) => {
                    if str::from_utf8(entry.payload).is_err() { return Err(StringBufferError::InvalidUtf8); }
                    cursor += adv;
                }
                Err(_) => return Err(StringBufferError::InvalidFormat),
            }
        }

        let next_offset = HEADER_SIZE + total_len;
        let remaining_capacity = buffer.len() - next_offset;
        Ok(Self {
            buffer,
            next_offset,
            remaining_capacity,
        })
    }

    /// Appends a string to the buffer.
    ///
    /// The string will be stored with a length prefix (u16) followed by the UTF-8 data.
    ///
    /// # Arguments
    /// * `s` - The string to append
    ///
    /// # Returns
    /// `Ok(())` if successful, or an error if the string cannot be added.
    pub fn append(&mut self, s: &str) -> Result<(), StringBufferError> {
        let string_bytes = s.as_bytes();
        let string_len = string_bytes.len();

        // Check if string is too long for u16 length prefix
        if string_len > u16::MAX as usize {
            return Err(StringBufferError::StringTooLong);
        }

    let required_space = Entry::encoded_len(string_len);
        if required_space > self.remaining_capacity {
            // Increment dropped counter in header
            let dropped = Self::read_dropped(self.buffer).saturating_add(1);
            Self::write_dropped(self.buffer, dropped);
            return Err(StringBufferError::BufferFull);
        }

    Entry::write(self.buffer, self.next_offset, string_bytes);

        // Update state
        self.next_offset += required_space;
        self.remaining_capacity -= required_space;
        // Update total_len in header (data region usage)
        let total_len = (self.next_offset - HEADER_SIZE) as u16;
        Self::write_total_len(self.buffer, total_len);

        Ok(())
    }

    /// Returns an iterator over all strings in the buffer.
    ///
    /// # Returns
    /// An iterator that yields `Result<&str, StringBufferError>` for each string.
    pub fn iter(&self) -> StringBufferIterator<'_> {
        StringBufferIterator {
            buffer: self.buffer,
            offset: HEADER_SIZE,
            end_offset: self.next_offset,
        }
    }

    /// Returns the number of bytes remaining in the buffer.
    pub fn remaining_capacity(&self) -> usize {
        self.remaining_capacity
    }

    /// Returns the number of bytes currently used in the buffer.
    pub fn used_capacity(&self) -> usize {
        self.next_offset
    }

    /// Returns true if the buffer is full.
    pub fn is_full(&self) -> bool {
        self.remaining_capacity == 0
    }

    /// Returns true if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        Self::read_total_len(self.buffer) == 0
    }

    /// Clears the buffer, removing all strings.
    pub fn clear(&mut self) {
        let total = self.buffer.len();
        self.buffer.fill(0);
        Self::write_total_len(self.buffer, 0);
        Self::write_dropped(self.buffer, 0);
        self.next_offset = HEADER_SIZE;
        self.remaining_capacity = total - HEADER_SIZE;
    }

    /// Returns a reference to the underlying buffer.
    pub fn as_bytes(&self) -> &[u8] {
        self.buffer
    }

    /// Returns number of dropped messages recorded in the header.
    pub fn dropped_messages(&self) -> u32 {
        Self::read_dropped(self.buffer)
    }

    /// Header helpers
    fn read_total_len(buf: &[u8]) -> u16 { Header::ref_from_prefix(buf).map(|h| h.total_len.get()).unwrap_or(0) }
    fn write_total_len(buf: &mut [u8], v: u16) { if let Some(h) = Header::mut_from_prefix(buf) { h.total_len = U16::new(v); } }
    fn read_dropped(buf: &[u8]) -> u32 { Header::ref_from_prefix(buf).map(|h| h.dropped.get()).unwrap_or(0) }
    fn write_dropped(buf: &mut [u8], v: u32) { if let Some(h) = Header::mut_from_prefix(buf) { h.dropped = U32::new(v); } }
}

/// Size of the header in bytes
const HEADER_SIZE: usize = 2 + 4; // total_len (u16) + dropped (u32)
/// Page size (4K) enforced for buffers
const PAGE_SIZE: usize = 4096;

#[repr(C, packed)]
#[derive(Debug, Copy, Clone, IntoBytes, Immutable, KnownLayout, FromBytes)]
struct Header {
    total_len: U16<LittleEndian>,
    dropped: U32<LittleEndian>,
}

impl Header {
    fn mut_from_prefix(buf: &mut [u8]) -> Option<&mut Self> { Self::mut_from_bytes(&mut buf[..HEADER_SIZE]).ok() }
    fn ref_from_prefix(buf: &[u8]) -> Option<&Self> { Self::ref_from_bytes(&buf[..HEADER_SIZE]).ok() }
}

/// Iterator over strings in a `StringBuffer`.
#[derive(Debug)]
pub struct StringBufferIterator<'a> {
    buffer: &'a [u8],
    offset: usize,
    end_offset: usize,
}

impl<'a> Iterator for StringBufferIterator<'a> {
    type Item = Result<&'a str, StringBufferError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.end_offset {
            return None;
        }

        match Entry::parse(&self.buffer[self.offset..self.end_offset]) {
            Ok((entry, adv)) => {
                self.offset += adv;
                Some(str::from_utf8(entry.payload).map_err(|_| StringBufferError::InvalidUtf8))
            }
            Err(_) => Some(Err(StringBufferError::InvalidFormat)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry<'a> { payload: &'a [u8] }

impl<'a> Entry<'a> {
    fn encoded_len(plen: usize) -> usize { 2 + plen }
    fn parse(buf: &'a [u8]) -> Result<(Entry<'a>, usize), ()> {
        if buf.len() < 2 { return Err(()); }
        let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
        if len == 0 || buf.len() < 2 + len { return Err(()); }
        Ok((Entry { payload: &buf[2..2+len] }, 2 + len))
    }
    fn write(dest: &mut [u8], offset: usize, payload: &[u8]) {
        let len = payload.len() as u16; let b = len.to_le_bytes();
        dest[offset]=b[0]; dest[offset+1]=b[1];
        dest[offset+2..offset+2+payload.len()].copy_from_slice(payload);
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
