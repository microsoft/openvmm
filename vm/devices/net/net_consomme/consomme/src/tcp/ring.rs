// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::num::Wrapping;
use std::ops::Range;

pub struct Ring {
    buf: Vec<u8>,
    head: Wrapping<usize>,
    tail: Wrapping<usize>,
}

impl Ring {
    pub fn new(n: usize) -> Self {
        assert!(n == 0 || n.is_power_of_two());
        Self {
            buf: vec![0; n],
            head: Wrapping(0),
            tail: Wrapping(0),
        }
    }

    pub fn consume(&mut self, n: usize) {
        assert!(self.tail - self.head >= Wrapping(n));
        self.head += n;
    }

    pub fn view(&self, range: Range<usize>) -> View<'_> {
        assert!(range.end <= self.len());
        View {
            buf: &self.buf,
            head: self.head + Wrapping(range.start),
            tail: self.head + Wrapping(range.end),
        }
    }

    #[cfg(test)]
    pub fn written_slices(&self) -> (&[u8], &[u8]) {
        self.view(0..self.len()).as_slices()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        (self.tail - self.head).0
    }

    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn unwritten_slices_mut(&mut self) -> (&mut [u8], &mut [u8]) {
        let mask = Wrapping(self.buf.len()) - Wrapping(1);
        let len = self.buf.len() - (self.tail - self.head).0;
        let start = (self.tail & mask).0;
        if start + len <= self.buf.len() {
            (&mut self.buf[start..start + len], &mut [])
        } else {
            let end = start + len - self.buf.len();
            let (buf, a) = self.buf.split_at_mut(start);
            let (b, _) = buf.split_at_mut(end);
            (a, b)
        }
    }

    pub fn extend_by(&mut self, n: usize) {
        assert!(self.capacity() - self.len() >= n);
        self.tail += n;
    }

    /// Grow the ring to `new_capacity`. Preserves the bytes in `[head, tail)`.
    ///
    /// This is a grow-only operation: `new_capacity` must be a power of two no
    /// smaller than the current capacity. A resize to the current capacity is a
    /// no-op, so no reallocation occurs.
    ///
    /// Any bytes written via [`Ring::write_at`] past `tail` (out-of-order data
    /// staged for a future `extend_by`) are NOT preserved. Callers using
    /// `write_at` must ensure no such staged data exists.
    pub fn resize(&mut self, new_capacity: usize) {
        // Copy `src` into `buf` (a power-of-two-sized buffer) starting at logical
        // offset `at`, wrapping around the end of `buf` as needed.
        fn place(buf: &mut [u8], at: usize, src: &[u8]) {
            if src.is_empty() {
                return;
            }
            let mask = buf.len() - 1;
            let start = at & mask;
            if start + src.len() <= buf.len() {
                buf[start..start + src.len()].copy_from_slice(src);
            } else {
                let mid = buf.len() - start;
                buf[start..].copy_from_slice(&src[..mid]);
                buf[..src.len() - mid].copy_from_slice(&src[mid..]);
            }
        }

        assert!(new_capacity.is_power_of_two());
        assert!(new_capacity >= self.capacity());
        if new_capacity == self.capacity() {
            return;
        }
        let len = self.len();
        let mut new_buf = vec![0u8; new_capacity];
        {
            // Copy the live `[head, tail)` bytes directly from the existing ring
            // into their new positions, without staging through a temporary
            // buffer. `head` keeps its logical value, so its physical offset in
            // the new buffer is `head & (new_capacity - 1)`.
            let (a, b) = self.view(0..len).as_slices();
            let start = self.head.0 & (new_capacity - 1);
            place(&mut new_buf, start, a);
            place(&mut new_buf, start + a.len(), b);
        }
        self.buf = new_buf;
    }

    /// Write `data` into the ring at `offset` bytes past `head`, without
    /// advancing `tail`. Used for both in-order and out-of-order writes.
    pub fn write_at(&mut self, offset: usize, data: &[u8]) {
        assert!(offset + data.len() <= self.capacity());
        if data.is_empty() {
            return;
        }
        let mask = self.buf.len() - 1;
        let start = (self.head + Wrapping(offset)).0 & mask;
        if start + data.len() <= self.buf.len() {
            self.buf[start..start + data.len()].copy_from_slice(data);
        } else {
            let mid = self.buf.len() - start;
            self.buf[start..].copy_from_slice(&data[..mid]);
            self.buf[..data.len() - mid].copy_from_slice(&data[mid..]);
        }
    }
}

#[derive(Clone)]
pub struct View<'a> {
    buf: &'a [u8],
    head: Wrapping<usize>,
    tail: Wrapping<usize>,
}

impl<'a> View<'a> {
    pub fn len(&self) -> usize {
        (self.tail - self.head).0
    }

    pub fn as_slices(&self) -> (&'a [u8], &'a [u8]) {
        let len = (self.tail - self.head).0;
        let mask = Wrapping(self.buf.len()) - Wrapping(1);
        let start = (self.head & mask).0;
        if start + len <= self.buf.len() {
            (&self.buf[start..start + len], &[])
        } else {
            let end = start + len - self.buf.len();
            let (buf, a) = self.buf.split_at(start);
            let (b, _) = buf.split_at(end);
            (a, b)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Ring;

    #[test]
    fn test_ring() {
        let mut ring = Ring::new(1024);
        assert_eq!(ring.capacity(), 1024);
        assert_eq!(ring.len(), 0);
        assert!(ring.is_empty());

        let (a, b) = ring.written_slices();
        assert!(a.is_empty());
        assert!(b.is_empty());

        let (a, b) = ring.unwritten_slices_mut();
        assert_eq!(a.len(), 1024);
        assert!(b.is_empty());
        for (i, c) in a.iter_mut().enumerate() {
            *c = i as u8;
        }

        ring.extend_by(10);
        let (a, b) = ring.written_slices();
        assert_eq!(a, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(b, &[]);

        ring.consume(5);
        let (a, b) = ring.written_slices();
        assert_eq!(a, &[5, 6, 7, 8, 9]);
        assert_eq!(b, &[]);

        let (a, b) = ring.unwritten_slices_mut();
        assert_eq!(a.len(), 1014);
        assert_eq!(b, &[0, 1, 2, 3, 4]);

        ring.extend_by(1016);
        ring.consume(500);
        let (a, b) = ring.written_slices();
        assert_eq!(a.len(), 519);
        assert_eq!(b, &[0, 1]);

        let (a, b) = ring.unwritten_slices_mut();
        assert_eq!(a.len(), 503);
        assert!(b.is_empty());
    }

    #[test]
    fn test_zero_capacity_ring() {
        let ring = Ring::new(0);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.capacity(), 0);
        assert!(ring.is_full());
        assert!(ring.is_empty());

        let view = ring.view(0..0);
        let (a, b) = view.as_slices();
        assert!(a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn test_zero_capacity_unwritten_slices() {
        let mut ring = Ring::new(0);
        let (a, b) = ring.unwritten_slices_mut();
        assert!(a.is_empty());
        assert!(b.is_empty());
    }

    #[test]
    fn test_write_at_no_wrap() {
        let mut ring = Ring::new(16);
        ring.write_at(0, &[1, 2, 3, 4]);
        ring.extend_by(4);
        let (a, b) = ring.written_slices();
        assert_eq!(a, &[1, 2, 3, 4]);
        assert!(b.is_empty());
    }

    #[test]
    fn test_write_at_with_wrap() {
        let mut ring = Ring::new(8);
        // Fill and consume most of the ring to position head near the end.
        ring.write_at(0, &[0; 6]);
        ring.extend_by(6);
        ring.consume(6);
        // Now head=6, capacity=8. Write 4 bytes at offset 0: wraps around.
        ring.write_at(0, &[10, 20, 30, 40]);
        ring.extend_by(4);
        let (a, b) = ring.written_slices();
        assert_eq!(a, &[10, 20]);
        assert_eq!(b, &[30, 40]);
    }

    #[test]
    fn test_write_at_capacity_boundary() {
        let mut ring = Ring::new(8);
        // Write exactly to capacity.
        ring.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        ring.extend_by(8);
        assert!(ring.is_full());
        let view = ring.view(0..8);
        let (a, b) = view.as_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_write_at_extend_consume_round_trip() {
        let mut ring = Ring::new(16);
        // Write data out of order, then fill the gap.
        ring.write_at(4, &[5, 6, 7, 8]);
        ring.write_at(0, &[1, 2, 3, 4]);
        ring.extend_by(8);
        let (a, b) = ring.written_slices();
        assert_eq!(a, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(b.is_empty());

        ring.consume(8);
        assert!(ring.is_empty());
    }

    #[test]
    fn test_write_at_overlapping() {
        let mut ring = Ring::new(16);
        ring.write_at(0, &[1, 2, 3, 4, 5, 6]);
        // Overwrite bytes 2..5 with new values.
        ring.write_at(2, &[30, 40, 50]);
        ring.extend_by(6);
        let (a, b) = ring.written_slices();
        assert_eq!(a, &[1, 2, 30, 40, 50, 6]);
        assert!(b.is_empty());
    }

    #[test]
    fn test_write_at_empty_data() {
        let mut ring = Ring::new(8);
        // Writing empty data should be a no-op.
        ring.write_at(0, &[]);
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn test_resize_empty() {
        let mut ring = Ring::new(8);
        ring.resize(64);
        assert_eq!(ring.capacity(), 64);
        assert_eq!(ring.len(), 0);
    }

    #[test]
    fn test_resize_no_wrap_before_no_wrap_after() {
        let mut ring = Ring::new(16);
        ring.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        ring.extend_by(8);
        ring.resize(64);
        assert_eq!(ring.capacity(), 64);
        assert_eq!(ring.len(), 8);
        let (a, b) = ring.written_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_resize_wrap_before_no_wrap_after() {
        // head=6, tail=14 (logically). 8-cap ring: physical layout wraps.
        let mut ring = Ring::new(8);
        ring.extend_by(6);
        ring.consume(6);
        ring.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        ring.extend_by(8);
        // In the old 8-byte buffer the data wraps at offset 6.
        let (a, b) = ring.written_slices();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 6);
        ring.resize(32);
        assert_eq!(ring.capacity(), 32);
        assert_eq!(ring.len(), 8);
        let (a, b) = ring.written_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_resize_same_capacity_noop_then_grow() {
        // A same-capacity resize must be a no-op that preserves data, even when
        // the live bytes physically wrap in the buffer. Then a real grow must
        // also preserve them.
        let mut ring = Ring::new(16);
        // Advance head so head.0 == 12 (within the old buffer).
        ring.extend_by(12);
        ring.consume(12);
        ring.write_at(0, &[1, 2, 3, 4, 5, 6, 7, 8]);
        ring.extend_by(8);
        // head & (16-1) == 12, so the data wraps: 4 bytes at 12..16, 4 at 0..4.
        let (a, b) = ring.written_slices();
        assert_eq!(a.len(), 4);
        assert_eq!(b.len(), 4);
        // Resize to the current capacity: no-op, data left in place.
        ring.resize(16);
        assert_eq!(ring.capacity(), 16);
        assert_eq!(ring.len(), 8);
        let (a, b) = ring.written_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        // Grow to 32: head.0 == 12, mask == 31, start == 12, 12 + 8 == 20, so
        // the data no longer wraps. It must still be preserved.
        ring.resize(32);
        assert_eq!(ring.capacity(), 32);
        assert_eq!(ring.len(), 8);
        let (a, b) = ring.written_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn test_resize_then_extend_uses_new_space() {
        let mut ring = Ring::new(8);
        ring.write_at(0, &[1, 2, 3, 4]);
        ring.extend_by(4);
        ring.resize(16);
        assert_eq!(ring.capacity(), 16);
        // Should have 12 bytes of free space now.
        let (a, b) = ring.unwritten_slices_mut();
        assert_eq!(a.len() + b.len(), 12);
        ring.write_at(4, &[5, 6, 7, 8, 9, 10, 11, 12]);
        ring.extend_by(8);
        let (a, b) = ring.written_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn test_resize_after_many_wraps() {
        // Drive head/tail counters well past any single capacity to make sure
        // resize relies on counter values modulo the new capacity, not the old.
        let mut ring = Ring::new(8);
        for _ in 0..100 {
            ring.write_at(0, &[42, 42, 42, 42]);
            ring.extend_by(4);
            ring.consume(4);
        }
        assert_eq!(ring.len(), 0);
        ring.write_at(0, &[1, 2, 3, 4, 5]);
        ring.extend_by(5);
        ring.resize(32);
        assert_eq!(ring.len(), 5);
        let (a, b) = ring.written_slices();
        let mut data = a.to_vec();
        data.extend_from_slice(b);
        assert_eq!(data, vec![1, 2, 3, 4, 5]);
    }
}
