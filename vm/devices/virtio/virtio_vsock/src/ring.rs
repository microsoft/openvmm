// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::IoSlice;
use std::io::Write;

/// A simple, single-threaded byte ring buffer with a fixed capacity.
pub struct RingBuffer {
    buf: Box<[u8]>,
    /// Index of the first readable byte.
    head: usize,
    /// Number of bytes currently stored.
    len: usize,
}

impl RingBuffer {
    /// Creates a new ring buffer that can hold up to `capacity` bytes.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0; capacity].into_boxed_slice(),
            head: 0,
            len: 0,
        }
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

    /// Writes bytes from the given I/O slices into the buffer, starting at byte `offset` into the
    /// logical concatenation of `bufs`.
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

    /// Writes the current contents of the ring buffer to `writer`, using `write_vectored` when the
    /// data wraps around. Advances the read position by the number of bytes written and returns
    /// that count.
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

        let buf = read_to_vec(&mut ring);
        assert_eq!(buf.len(), 5);
        assert_eq!(&buf, b"hello");
        assert!(ring.is_empty());
    }

    #[test]
    fn write_wraps_around() {
        let mut ring = RingBuffer::new(8);
        write_bytes(&mut ring, b"abcdef");
        let buf = read_to_vec(&mut ring);
        assert_eq!(&buf, b"abcdef");
        write_bytes(&mut ring, b"ghijk");
        assert_eq!(ring.len(), 5);

        let buf = read_to_vec(&mut ring);
        assert_eq!(&buf, b"ghijk");
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

        let buf = read_to_vec(&mut ring);
        assert_eq!(&buf, b"hello world");
    }

    #[test]
    fn write_with_offset_skips_bytes() {
        let mut ring = RingBuffer::new(16);
        // "hello world" with offset 6 => "world"
        let a = b"hello ";
        let b = b"world";
        ring.write(&[IoSlice::new(a), IoSlice::new(b)], 6);
        assert_eq!(ring.len(), 5);

        let buf = read_to_vec(&mut ring);
        assert_eq!(&buf, b"world");
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

        let buf = read_to_vec(&mut ring);
        assert_eq!(&buf, b"defgh");
    }

    #[test]
    fn write_with_offset_equal_to_total_writes_nothing() {
        let mut ring = RingBuffer::new(8);
        ring.write(&[IoSlice::new(b"abc")], 3);
        assert!(ring.is_empty());
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
        write_bytes(&mut ring, b"abcd");
        read_to_vec(&mut ring);
        write_bytes(&mut ring, b"efghij"); // wraps: buf=[i,j,_,_,e,f,g,h]
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

    fn read_to_vec(ring: &mut RingBuffer) -> Vec<u8> {
        let mut out = Vec::new();
        let size = ring.read_to(&mut out).unwrap();
        out.truncate(size);
        out
    }
}
