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

    /// Copies the view contents into `buf`.
    ///
    /// # Panics
    /// Panics if `buf` is smaller than the view length.
    pub fn copy_to_slice(&self, buf: &mut [u8]) {
        let (a, b) = self.as_slices();
        buf[..a.len()].copy_from_slice(a);
        buf[a.len()..a.len() + b.len()].copy_from_slice(b);
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
}
