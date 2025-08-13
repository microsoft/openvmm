// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! A 4K length-prefixed string buffer for storing logs.
//!
//! This crate provides a no_std compatible string buffer that stores
//! length-prefixed strings in a fixed 4K buffer. It supports both reading
//! existing buffers and appending new strings until the buffer is full.

#![no_std]
#![forbid(unsafe_code)]

use core::str;

/// Size of the string buffer in bytes (4KB).
pub const BUFFER_SIZE: usize = 4096;

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
    /// Reference to the 4K storage buffer
    buffer: &'a mut [u8; BUFFER_SIZE],
    /// The next byte offset where the next string should be inserted
    next_offset: usize,
    /// Remaining capacity in bytes
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
    pub fn new(buffer: &'a mut [u8; BUFFER_SIZE]) -> Self {
        // Initialize buffer with zeros to ensure clean state
        buffer.fill(0);
        
        Self {
            buffer,
            next_offset: 0,
            remaining_capacity: BUFFER_SIZE,
        }
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
    pub fn from_existing(buffer: &'a mut [u8; BUFFER_SIZE]) -> Result<Self, StringBufferError> {
        let mut offset = 0;
        
        // Parse existing strings to find the end of data
        while offset < BUFFER_SIZE {
            // Check if we've reached the end (null terminator or uninitialized data)
            if offset + 2 > BUFFER_SIZE || buffer[offset] == 0 && buffer[offset + 1] == 0 {
                break;
            }
            
            // Read length prefix (u16 little-endian)
            let length = u16::from_le_bytes([buffer[offset], buffer[offset + 1]]) as usize;
            offset += 2;
            
            // Validate length
            if length == 0 || offset + length > BUFFER_SIZE {
                return Err(StringBufferError::InvalidFormat);
            }
            
            // Validate UTF-8
            str::from_utf8(&buffer[offset..offset + length])
                .map_err(|_| StringBufferError::InvalidUtf8)?;
            
            offset += length;
        }
        
        let remaining_capacity = BUFFER_SIZE - offset;
        
        Ok(Self {
            buffer,
            next_offset: offset,
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
        
        // Check if we have enough space (2 bytes for length + string bytes)
        let required_space = 2 + string_len;
        if required_space > self.remaining_capacity {
            return Err(StringBufferError::BufferFull);
        }
        
        // Write length prefix (u16 little-endian)
        let length_bytes = (string_len as u16).to_le_bytes();
        self.buffer[self.next_offset] = length_bytes[0];
        self.buffer[self.next_offset + 1] = length_bytes[1];
        
        // Write string data
        self.buffer[self.next_offset + 2..self.next_offset + 2 + string_len]
            .copy_from_slice(string_bytes);
        
        // Update state
        self.next_offset += required_space;
        self.remaining_capacity -= required_space;
        
        Ok(())
    }

    /// Returns an iterator over all strings in the buffer.
    ///
    /// # Returns
    /// An iterator that yields `Result<&str, StringBufferError>` for each string.
    pub fn iter(&self) -> StringBufferIterator<'_> {
        StringBufferIterator {
            buffer: self.buffer,
            offset: 0,
            end_offset: self.next_offset,
        }
    }

    /// Returns the number of bytes remaining in the buffer.
    pub fn remaining_capacity(&self) -> usize {
        self.remaining_capacity
    }

    /// Returns the number of bytes currently used in the buffer.
    pub fn used_capacity(&self) -> usize {
        BUFFER_SIZE - self.remaining_capacity
    }

    /// Returns true if the buffer is full.
    pub fn is_full(&self) -> bool {
        self.remaining_capacity == 0
    }

    /// Returns true if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.next_offset == 0
    }

    /// Clears the buffer, removing all strings.
    pub fn clear(&mut self) {
        self.buffer.fill(0);
        self.next_offset = 0;
        self.remaining_capacity = BUFFER_SIZE;
    }

    /// Returns a reference to the underlying buffer.
    pub fn as_bytes(&self) -> &[u8; BUFFER_SIZE] {
        self.buffer
    }
}

/// Iterator over strings in a `StringBuffer`.
#[derive(Debug)]
pub struct StringBufferIterator<'a> {
    buffer: &'a [u8; BUFFER_SIZE],
    offset: usize,
    end_offset: usize,
}

impl<'a> Iterator for StringBufferIterator<'a> {
    type Item = Result<&'a str, StringBufferError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset >= self.end_offset {
            return None;
        }

        // Check if we have at least 2 bytes for the length prefix
        if self.offset + 2 > self.end_offset {
            return Some(Err(StringBufferError::InvalidFormat));
        }

        // Read length prefix
        let length = u16::from_le_bytes([
            self.buffer[self.offset],
            self.buffer[self.offset + 1],
        ]) as usize;
        self.offset += 2;

        // Check if we have enough bytes for the string
        if self.offset + length > self.end_offset {
            return Some(Err(StringBufferError::InvalidFormat));
        }

        // Extract string data and validate UTF-8
        let string_bytes = &self.buffer[self.offset..self.offset + length];
        let result = str::from_utf8(string_bytes)
            .map_err(|_| StringBufferError::InvalidUtf8);

        self.offset += length;
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::{vec, vec::Vec};

    #[test]
    fn test_new_buffer() {
        let mut storage = [0u8; BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage);
        
        assert_eq!(buffer.remaining_capacity(), BUFFER_SIZE);
        assert_eq!(buffer.used_capacity(), 0);
        assert!(buffer.is_empty());
        assert!(!buffer.is_full());
    }

    #[test]
    fn test_append_string() {
        let mut storage = [0u8; BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage);
        
        let test_string = "Hello, World!";
        assert!(buffer.append(test_string).is_ok());
        
        assert_eq!(buffer.remaining_capacity(), BUFFER_SIZE - 2 - test_string.len());
        assert_eq!(buffer.used_capacity(), 2 + test_string.len());
        assert!(!buffer.is_empty());
        assert!(!buffer.is_full());
    }

    #[test]
    fn test_append_multiple_strings() {
        let mut storage = [0u8; BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage);
        
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
        let mut storage = [0u8; BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage);
        
        // Try to create a string that's larger than u16::MAX
        let large_string = "x".repeat(70000);
        let result = buffer.append(&large_string);
        assert_eq!(result, Err(StringBufferError::StringTooLong));
        
        // Fill buffer with maximum possible string (accounting for 2-byte length prefix)
        let max_string = "x".repeat(BUFFER_SIZE - 2);
        assert!(buffer.append(&max_string).is_ok());
        assert!(buffer.is_full());
        
        // Try to append another string
        let result = buffer.append("test");
        assert_eq!(result, Err(StringBufferError::BufferFull));
    }

    #[test]
    fn test_from_existing_empty() {
        let mut storage = [0u8; BUFFER_SIZE];
        let buffer = StringBuffer::from_existing(&mut storage);
        
        assert!(buffer.is_ok());
        let buffer = buffer.unwrap();
        assert_eq!(buffer.remaining_capacity(), BUFFER_SIZE);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_from_existing_with_data() {
        let mut storage = [0u8; BUFFER_SIZE];
        
        // Manually create some test data
        let test_string = "Hello";
        let length_bytes = (test_string.len() as u16).to_le_bytes();
        storage[0] = length_bytes[0];
        storage[1] = length_bytes[1];
        storage[2..2 + test_string.len()].copy_from_slice(test_string.as_bytes());
        
        let buffer = StringBuffer::from_existing(&mut storage);
        assert!(buffer.is_ok());
        
        let buffer = buffer.unwrap();
        let strings: Result<Vec<&str>, _> = buffer.iter().collect();
        assert!(strings.is_ok());
        assert_eq!(strings.unwrap(), vec!["Hello"]);
    }

    #[test]
    fn test_clear() {
        let mut storage = [0u8; BUFFER_SIZE];
        let mut buffer = StringBuffer::new(&mut storage);
        
        assert!(buffer.append("test").is_ok());
        assert!(!buffer.is_empty());
        
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.remaining_capacity(), BUFFER_SIZE);
    }

    #[test]
    fn test_iterator_empty() {
        let mut storage = [0u8; BUFFER_SIZE];
        let buffer = StringBuffer::new(&mut storage);
        
        let strings: Vec<_> = buffer.iter().collect();
        assert!(strings.is_empty());
    }
}
