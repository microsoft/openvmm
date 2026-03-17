// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::IoSlice;
use std::io::Write;

/// A simple, single-threaded byte ring buffer with a fixed capacity.
pub struct RingBuffer {
    buf: Vec<u8>,
    /// Index of the first readable byte.
    head: usize,
    /// Number of bytes currently stored.
    len: usize,
}

impl RingBuffer {
    /// Creates a new ring buffer that can hold up to `capacity` bytes.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0; capacity],
            head: 0,
            len: 0,
        }
    }

    /// Total capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of bytes currently stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if the buffer contains no data.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of bytes that can still be written before the buffer is full.
    pub fn available(&self) -> usize {
        self.buf.len() - self.len
    }

    /// Writes bytes from the given I/O slices into the buffer, starting at
    /// byte `offset` into the logical concatenation of `bufs`.
    ///
    /// All bytes from `offset` to the end of the slices are written.
    ///
    /// # Panics
    ///
    /// Panics if the number of bytes to write exceeds `self.available()`.
    pub fn write(&mut self, bufs: &[IoSlice<'_>], offset: usize) {
        let mut skip = offset;
        let mut tail = (self.head + self.len) % self.buf.len();
        let mut written = 0;

        for slice in bufs {
            let slice: &[u8] = slice;
            if skip >= slice.len() {
                skip -= slice.len();
                continue;
            }
            let chunk = &slice[skip..];
            skip = 0;

            assert!(
                chunk.len() <= self.available() - written,
                "write of {} bytes exceeds available space of {}",
                chunk.len(),
                self.available() - written,
            );

            let first = chunk.len().min(self.buf.len() - tail);
            self.buf[tail..tail + first].copy_from_slice(&chunk[..first]);
            let second = chunk.len() - first;
            if second > 0 {
                self.buf[..second].copy_from_slice(&chunk[first..]);
            }
            tail = (tail + chunk.len()) % self.buf.len();
            written += chunk.len();
        }

        self.len += written;
    }

    /// Reads up to `buf.len()` bytes into `buf`. Returns the number of bytes
    /// actually read.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        let to_read = buf.len().min(self.len);

        let first = to_read.min(self.buf.len() - self.head);
        buf[..first].copy_from_slice(&self.buf[self.head..self.head + first]);

        let second = to_read - first;
        if second > 0 {
            buf[first..first + second].copy_from_slice(&self.buf[..second]);
        }

        self.head = (self.head + to_read) % self.buf.len();
        self.len -= to_read;
        to_read
    }

    /// Writes the current contents of the ring buffer to `writer`, using
    /// `write_vectored` when the data wraps around. Advances the read
    /// position by the number of bytes written and returns that count.
    pub fn read_to(&mut self, writer: &mut impl Write) -> std::io::Result<usize> {
        let mut total_written = 0;

        while !self.is_empty() {
            let first_end = (self.head + self.len).min(self.buf.len());
            let first = &self.buf[self.head..first_end];
            let written = if first.len() < self.len {
                // Data wraps around — use write_vectored with two slices.
                let second = &self.buf[..self.len - first.len()];
                writer.write_vectored(&[IoSlice::new(first), IoSlice::new(second)])?
            } else {
                writer.write(first)?
            };

            self.head = (self.head + written) % self.buf.len();
            self.len -= written;
            total_written += written;
        }

        Ok(total_written)
    }

    /// Discards up to `count` bytes from the front. Returns the number of
    /// bytes actually discarded.
    pub fn skip(&mut self, count: usize) -> usize {
        let to_skip = count.min(self.len);
        self.head = (self.head + to_skip) % self.buf.len();
        self.len -= to_skip;
        to_skip
    }

    /// Resets the buffer to empty without changing its capacity.
    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write a single byte slice with no offset.
    fn write_bytes(ring: &mut RingBuffer, data: &[u8]) {
        ring.write(&[IoSlice::new(data)], 0);
    }

    #[test]
    fn new_buffer_is_empty() {
        let ring = RingBuffer::new(16);
        assert_eq!(ring.capacity(), 16);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());
        assert_eq!(ring.available(), 16);
    }

    #[test]
    fn write_and_read() {
        let mut ring = RingBuffer::new(8);
        write_bytes(&mut ring, b"hello");
        assert_eq!(ring.len(), 5);
        assert_eq!(ring.available(), 3);

        let mut buf = [0u8; 8];
        assert_eq!(ring.read(&mut buf), 5);
        assert_eq!(&buf[..5], b"hello");
        assert!(ring.is_empty());
    }

    #[test]
    fn write_wraps_around() {
        let mut ring = RingBuffer::new(8);
        write_bytes(&mut ring, b"abcdef");
        let mut tmp = [0u8; 4];
        assert_eq!(ring.read(&mut tmp), 4);
        assert_eq!(&tmp, b"abcd");
        write_bytes(&mut ring, b"ghijk");
        assert_eq!(ring.len(), 7);

        let mut out = [0u8; 7];
        assert_eq!(ring.read(&mut out), 7);
        assert_eq!(&out, b"efghijk");
    }

    #[test]
    #[should_panic(expected = "write of 6 bytes exceeds available space of 4")]
    fn write_panics_when_overflowing() {
        let mut ring = RingBuffer::new(4);
        write_bytes(&mut ring, b"abcdef");
    }

    #[test]
    #[should_panic(expected = "exceeds available space")]
    fn write_panics_when_full() {
        let mut ring = RingBuffer::new(4);
        write_bytes(&mut ring, b"abcd");
        write_bytes(&mut ring, b"x");
    }

    #[test]
    fn write_multiple_slices() {
        let mut ring = RingBuffer::new(16);
        let a = b"hello";
        let b = b" world";
        ring.write(&[IoSlice::new(a), IoSlice::new(b)], 0);
        assert_eq!(ring.len(), 11);

        let mut out = [0u8; 11];
        ring.read(&mut out);
        assert_eq!(&out, b"hello world");
    }

    #[test]
    fn write_with_offset_skips_bytes() {
        let mut ring = RingBuffer::new(16);
        // "hello world" with offset 6 => "world"
        let a = b"hello ";
        let b = b"world";
        ring.write(&[IoSlice::new(a), IoSlice::new(b)], 6);
        assert_eq!(ring.len(), 5);

        let mut out = [0u8; 5];
        ring.read(&mut out);
        assert_eq!(&out, b"world");
    }

    #[test]
    fn write_with_offset_spanning_slices() {
        let mut ring = RingBuffer::new(16);
        // offset 3 into ["ab", "cdef", "gh"] => skip "ab" + 1 byte of "cdef" => "defgh"
        let s1 = b"ab";
        let s2 = b"cdef";
        let s3 = b"gh";
        ring.write(&[IoSlice::new(s1), IoSlice::new(s2), IoSlice::new(s3)], 3);
        assert_eq!(ring.len(), 5);

        let mut out = [0u8; 5];
        ring.read(&mut out);
        assert_eq!(&out, b"defgh");
    }

    #[test]
    fn write_with_offset_equal_to_total_writes_nothing() {
        let mut ring = RingBuffer::new(8);
        ring.write(&[IoSlice::new(b"abc")], 3);
        assert!(ring.is_empty());
    }

    #[test]
    fn skip_discards_bytes() {
        let mut ring = RingBuffer::new(8);
        write_bytes(&mut ring, b"abcdef");
        assert_eq!(ring.skip(3), 3);
        assert_eq!(ring.len(), 3);

        let mut buf = [0u8; 3];
        ring.read(&mut buf);
        assert_eq!(&buf, b"def");
    }

    #[test]
    fn clear_resets() {
        let mut ring = RingBuffer::new(8);
        write_bytes(&mut ring, b"data");
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.available(), 8);
    }

    #[test]
    fn read_to_contiguous() {
        let mut ring = RingBuffer::new(16);
        write_bytes(&mut ring, b"hello");
        let mut out = Vec::new();
        let n = ring.read_to(&mut out).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&out, b"hello");
        assert!(ring.is_empty());
    }

    #[test]
    fn read_to_wrapped() {
        let mut ring = RingBuffer::new(8);
        // Fill and partially drain to move head forward.
        write_bytes(&mut ring, b"abcdef");
        ring.skip(4); // head=4, data="ef"
        write_bytes(&mut ring, b"ghij"); // wraps: buf=[i,j,_,_,e,f,g,h]
        assert_eq!(ring.len(), 6);

        let mut out = Vec::new();
        let n = ring.read_to(&mut out).unwrap();
        assert_eq!(n, 6);
        assert_eq!(&out, b"efghij");
        assert!(ring.is_empty());
    }

    #[test]
    fn read_to_empty() {
        let mut ring = RingBuffer::new(8);
        let mut out = Vec::new();
        let n = ring.read_to(&mut out).unwrap();
        assert_eq!(n, 0);
        assert!(out.is_empty());
    }
}
