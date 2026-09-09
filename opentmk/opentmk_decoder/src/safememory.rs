// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use core::ops::{Deref, DerefMut};

use alloc::{boxed::Box, format, string::String, vec::Vec};

/// This represents a virtual memory map that can be used to safely write/read from
/// memory for syzkaller calls that use executor memory
pub trait SafeMemoryMap: Send + Sync {
    /// Writes some bytes to the memory map
    ///
    /// # Panics
    /// If we do not do a full write to memory
    #[inline]
    fn write_mem(&mut self, base: usize, val: &[u8]) {
        self.try_write_mem(base, val)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Reads some bytes from the memory map
    ///
    /// # Panics
    /// If we do not do a full read from memory
    #[inline]
    fn read_mem(&mut self, base: usize, val: &mut [u8]) {
        self.try_read_mem(base, val)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    /// Attempts to write some bytes to the memory map, returning an error message
    /// if it fails
    #[inline]
    fn try_write_mem(&mut self, base: usize, val: &[u8]) -> Result<(), String> {
        let written = self.partial_write_mem(base, val);
        if written == val.len() {
            Ok(())
        } else {
            Err(format!(
                "SafeMemoryMap: did not do full write at 0x{base:016x}, written: {written} / {} bytes",
                val.len()
            ))
        }
    }

    /// Attempts to read some bytes from the memory map, returning an error message
    /// if it fails
    #[inline]
    fn try_read_mem(&mut self, base: usize, val: &mut [u8]) -> Result<(), String> {
        let read = self.partial_read_mem(base, val);
        if read == val.len() {
            Ok(())
        } else {
            Err(format!(
                "SafeMemoryMap: did not do full read at 0x{base:016x}, read: {read} / {} bytes",
                val.len()
            ))
        }
    }

    /// Writes some bytes to the memory map, returning the number of bytes actually
    /// written
    #[must_use]
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize;

    /// Reads some bytes from the memory map, returning the number of bytes
    /// actually read
    #[must_use]
    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize;
}

impl<T: SafeMemoryMap + ?Sized> SafeMemoryMap for Box<T> {
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        (**self).partial_write_mem(base, val)
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        (**self).partial_read_mem(base, val)
    }
}

impl<T: SafeMemoryMap + ?Sized> SafeMemoryMap for &mut T {
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        (**self).partial_write_mem(base, val)
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        (**self).partial_read_mem(base, val)
    }
}

/// Without an explicit address key value, we implicitly use the real address
/// base of the u8 vec to compute from.
impl SafeMemoryMap for Vec<u8> {
    #[inline]
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        let addr = self.as_ptr() as usize;
        SingleMap::from_slice(self, addr).partial_write_mem(base, val)
    }

    #[inline]
    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        let addr = self.as_ptr() as usize;
        SingleMap::from_slice(self, addr).partial_read_mem(base, val)
    }
}

/// Without an explicit address key value, we implicitly use the real address
/// base of the u8 slice to compute from.
impl SafeMemoryMap for [u8] {
    #[inline]
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        let addr = self.as_ptr() as usize;
        SingleMap::from_slice(self, addr).partial_write_mem(base, val)
    }

    #[inline]
    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        let addr = self.as_ptr() as usize;
        SingleMap::from_slice(self, addr).partial_read_mem(base, val)
    }
}

/// Without an explicit address key value, we implicitly use the real address
/// base of the u8 slice to compute from.
impl<const LEN: usize> SafeMemoryMap for [u8; LEN] {
    #[inline]
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        (self as &mut [u8]).partial_write_mem(base, val)
    }

    #[inline]
    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        (self as &mut [u8]).partial_read_mem(base, val)
    }
}

/// Represents a single memory map
pub struct SingleMap<B> {
    data: B,
    /// The virtual address that corresponds to the beginning of the data
    pub offset: usize,
}

impl<B> Deref for SingleMap<B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<B> DerefMut for SingleMap<B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl<B> SingleMap<B>
where
    B: DerefMut<Target = [u8]> + Send + Sync,
{
    /// Create a `SingleMap` from any data entity that can deref into a
    /// u8 slice (i.e. `Vec<u8>`, `&mut [u8]`, etc...). Note that if you pass
    /// a sized slice, i.e. `&mut [1, 2, 3, 4]` you should use
    /// [`.from_slice()`](Self::from_slice) instead.
    pub fn new(data: B, offset: usize) -> Self {
        Self { data, offset }
    }
}

impl<'a> SingleMap<&'a mut [u8]> {
    /// Create a `SingleMap` from a `mut [u8]` slice
    pub fn from_slice(data: &'a mut [u8], offset: usize) -> Self {
        Self { data, offset }
    }
}

impl<B> SafeMemoryMap for SingleMap<B>
where
    B: DerefMut<Target = [u8]> + Send + Sync,
{
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        let Some(off) = base.checked_sub(self.offset) else {
            return 0;
        };
        let Some(arr_len) = self.data.len().checked_sub(off) else {
            return 0;
        };
        let written = val.len().min(arr_len);
        self.data[off..off + written].copy_from_slice(&val[..written]);
        written
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        let Some(off) = base.checked_sub(self.offset) else {
            return 0;
        };
        let Some(arr_len) = self.data.len().checked_sub(off) else {
            return 0;
        };
        let written = val.len().min(arr_len);
        val[..written].copy_from_slice(&self.data[off..off + written]);
        written
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_round_trip_box() {
        let mut buf = [0u8, 1, 2, 3];
        let mut buf = Box::new(SingleMap::from_slice(&mut buf, 0xdead0000usize));
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_mut_box_box() {
        let mut buf = [0u8, 1, 2, 3];
        let mut buf = Box::new(SingleMap::from_slice(&mut buf, 0xdead0000usize));
        let mut buf = Box::new(&mut buf);
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_u8_slice() {
        let buf = &mut [0u8, 1, 2, 3];
        let base = buf.as_ptr() as usize;
        buf.write_mem(base + 1, &[2]);

        let mut res = [0; 4];
        buf.read_mem(base, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_u8_slice2() {
        let buf = &mut [0u8, 1, 2, 3] as &mut [u8];
        let base = buf.as_ptr() as usize;
        buf.write_mem(base + 1, &[2]);

        let mut res = [0; 4];
        buf.read_mem(base, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_u8_slice_addr() {
        let buf = &mut [0u8, 1, 2, 3] as &mut [u8];
        let mut buf = Box::new(SingleMap::from_slice(buf, 0xdead0000usize));
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_u8_vec_addr() {
        let mut buf = SingleMap::new(vec![0u8, 1, 2, 3], 0xdead0000usize);
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_write() {
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_write_partial() {
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        assert_eq!(3, buf.partial_write_mem(0xdead0001, &[2, 3, 4, 5]));
        assert_eq!(&[0, 2, 3, 4], buf.data);
    }

    #[test]
    fn test_read_partial() {
        let mut res = [0xff; 4];
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        assert_eq!(3, buf.partial_read_mem(0xdead0001, &mut res));
        assert_eq!(res, [1, 2, 3, 0xff]);
    }

    #[test]
    #[should_panic]
    fn test_write_panic() {
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        buf.write_mem(0xdeadbeef, &[2]);
    }

    #[test]
    #[should_panic]
    fn test_read_panic() {
        let mut res = [0xff];
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        buf.read_mem(0xdeadbeef, &mut res);
    }

    #[test]
    fn test_write_fail() {
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        assert!(buf.try_write_mem(0xdeadbeef, &[2]).is_err());
    }

    #[test]
    fn test_read_fail() {
        let mut res = [0xff];
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        assert!(buf.try_read_mem(0xdeadbeef, &mut res).is_err());
    }

    #[test]
    fn test_try_write_success() {
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        assert!(buf.try_write_mem(0xdead0001, &[2]).is_ok());
    }

    #[test]
    fn test_try_read_success() {
        let mut res = [0xff];
        let buf = &mut [0u8, 1, 2, 3];
        let mut buf = SingleMap::from_slice(buf, 0xdead0000usize);
        assert!(buf.try_read_mem(0xdead0001, &mut res).is_ok());
    }
}
