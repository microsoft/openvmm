// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

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

impl SafeMemoryMap for (&mut [u8], usize) {
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        let Some(off) = base.checked_sub(self.1) else {
            return 0;
        };
        let Some(arr_len) = self.0.len().checked_sub(off) else {
            return 0;
        };
        let written = val.len().min(arr_len);
        self.0[off..off + written].copy_from_slice(&val[..written]);
        written
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        let Some(off) = base.checked_sub(self.1) else {
            return 0;
        };
        let Some(arr_len) = self.0.len().checked_sub(off) else {
            return 0;
        };
        let written = val.len().min(arr_len);
        val[..written].copy_from_slice(&self.0[off..off + written]);
        written
    }
}

impl<const SIZE: usize> SafeMemoryMap for (&mut [u8; SIZE], usize) {
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        (self.0 as &mut [u8], self.1).partial_write_mem(base, val)
    }

    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        (self.0 as &mut [u8], self.1).partial_read_mem(base, val)
    }
}

impl SafeMemoryMap for (Vec<u8>, usize) {
    #[inline]
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        (self.0.as_mut_slice(), self.1).partial_write_mem(base, val)
    }

    #[inline]
    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        (self.0.as_mut_slice(), self.1).partial_read_mem(base, val)
    }
}

/// Without an explicit address key value, we implicitly use the real address
/// base of the u8 slice to compute from.
impl SafeMemoryMap for [u8] {
    #[inline]
    fn partial_write_mem(&mut self, base: usize, val: &[u8]) -> usize {
        let addr = self.as_ptr() as usize;
        (self, addr).partial_write_mem(base, val)
    }

    #[inline]
    fn partial_read_mem(&mut self, base: usize, val: &mut [u8]) -> usize {
        let addr = self.as_ptr() as usize;
        (self, addr).partial_read_mem(base, val)
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_round_trip_box() {
        let mut buf = [0u8, 1, 2, 3];
        let mut buf = Box::new((&mut buf, 0xdead0000usize));
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_mut_box_box() {
        let mut buf = [0u8, 1, 2, 3];
        let mut buf = Box::new((&mut buf, 0xdead0000usize));
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
        let mut buf = Box::new((buf, 0xdead0000usize));
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_round_trip_u8_vec_addr() {
        let mut buf = (vec![0u8, 1, 2, 3], 0xdead0000usize);
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_write() {
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        buf.write_mem(0xdead0001, &[2]);

        let mut res = [0; 4];
        buf.read_mem(0xdead0000, &mut res);
        assert_eq!([0, 2, 2, 3], res);
    }

    #[test]
    fn test_write_partial() {
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        assert_eq!(3, buf.partial_write_mem(0xdead0001, &[2, 3, 4, 5]));
        assert_eq!(&[0, 2, 3, 4], buf.0);
    }

    #[test]
    fn test_read_partial() {
        let mut res = [0xff; 4];
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        assert_eq!(3, buf.partial_read_mem(0xdead0001, &mut res));
        assert_eq!(res, [1, 2, 3, 0xff]);
    }

    #[test]
    #[should_panic]
    fn test_write_panic() {
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        buf.write_mem(0xdeadbeef, &[2]);
    }

    #[test]
    #[should_panic]
    fn test_read_panic() {
        let mut res = [0xff];
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        buf.read_mem(0xdeadbeef, &mut res);
    }

    #[test]
    fn test_write_fail() {
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        assert!(buf.try_write_mem(0xdeadbeef, &[2]).is_err());
    }

    #[test]
    fn test_read_fail() {
        let mut res = [0xff];
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        assert!(buf.try_read_mem(0xdeadbeef, &mut res).is_err());
    }

    #[test]
    fn test_try_write_success() {
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        assert!(buf.try_write_mem(0xdead0001, &[2]).is_ok());
    }

    #[test]
    fn test_try_read_success() {
        let mut res = [0xff];
        let mut buf = (&mut [0u8, 1, 2, 3], 0xdead0000usize);
        assert!(buf.try_read_mem(0xdead0001, &mut res).is_ok());
    }
}
