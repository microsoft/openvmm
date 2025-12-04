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
        assert!(n.is_power_of_two());
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
        let mask = Wrapping(self.buf.len() - 1);
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
        let mask = Wrapping(self.buf.len() - 1);
        let len = (self.tail - self.head).0;
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

    pub fn iter(&self) -> impl '_ + Iterator<Item = &u8> {
        let (a, b) = self.as_slices();
        a.iter().chain(b)
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
}
