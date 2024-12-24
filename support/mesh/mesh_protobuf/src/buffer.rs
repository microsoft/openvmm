// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Types to support writing to a contiguous byte buffer.
//!
//! This is different from `bytes::BufMut` in that the buffer is required to be
//! contiguous, which allows for more efficient use with type erasure.

use alloc::vec::Vec;
use core::mem::MaybeUninit;

/// Models a partially written, contiguous byte buffer.
pub trait Buffer {
    /// Returns the unwritten portion of the buffer. The returned data may or
    /// may not be initialized.
    ///
    /// # Safety
    /// The caller must ensure that no uninitialized bytes are written to the
    /// slice.
    ///
    /// An astute reader might note that the `Vec<u8>` implementation does not
    /// require the unsafe bound on this function, as those bytes returned by
    /// are truly `MaybeUninit`. However, based on the backing storage of [Buffer]
    /// this is not always the case.
    ///
    /// For example, a `Buffer` implementation on a `Cursor<&[u8]>` could be used
    /// to _uninitialize_ a portion of the slice, by doing the following:
    ///
    /// ```ignore
    /// // some_cursor contains a Cursor based implementation of Buffer which is
    /// // backed by storage that is always initialized.
    /// let foo = some_cursor.unwritten();
    /// foo[0].write(MaybeUninit::uninit()) // This is UB!! ⚠️
    /// ```
    ///
    /// Thus the caller must ensure that uninitialize bytes are _never_
    /// written to the returned slice, and why this function is unsafe.
    unsafe fn unwritten(&mut self) -> &mut [MaybeUninit<u8>];

    /// Extends the initialized region of the buffer.
    ///
    /// # Safety
    /// The caller must ensure that the next `len` bytes have been initialized.
    unsafe fn extend_written(&mut self, len: usize);
}

impl Buffer for Vec<u8> {
    unsafe fn unwritten(&mut self) -> &mut [MaybeUninit<u8>] {
        self.spare_capacity_mut()
    }

    unsafe fn extend_written(&mut self, len: usize) {
        // SAFETY: The caller guarantees that `len` bytes have been written.
        unsafe {
            self.set_len(self.len() + len);
        }
    }
}

impl Buffer for Buf<'_> {
    unsafe fn unwritten(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.buf[*self.filled..]
    }

    unsafe fn extend_written(&mut self, len: usize) {
        *self.filled += len;
    }
}

#[cfg(feature = "std")]
impl Buffer for std::io::Cursor<&mut [u8]> {
    unsafe fn unwritten(&mut self) -> &mut [MaybeUninit<u8>] {
        let slice = self.get_mut();
        // SAFETY: the caller promises not to uninitialize any initialized data.
        unsafe { core::slice::from_raw_parts_mut(slice.as_mut_ptr().cast(), slice.len()) }
    }

    unsafe fn extend_written(&mut self, len: usize) {
        self.set_position(self.position() + len as u64);
    }
}

/// An accessor for writing to a partially-initialized byte buffer.
pub struct Buf<'a> {
    buf: &'a mut [MaybeUninit<u8>],
    filled: &'a mut usize,
}

impl Buf<'_> {
    /// Returns the remaining bytes that fit.
    #[inline(always)]
    pub fn remaining(&self) -> usize {
        self.buf.len() - *self.filled
    }

    /// Returns the number of bytes that have been written.
    #[inline(always)]
    pub fn len(&self) -> usize {
        *self.filled
    }

    /// Extends the initialized portion of the buffer with `b`. Panics if it
    /// doesn't fit.
    #[inline(always)]
    pub fn push(&mut self, b: u8) {
        self.buf[*self.filled] = MaybeUninit::new(b);
        *self.filled += 1;
    }

    /// Extends the initialized portion of the buffer with `buf`. Panics if the
    /// data does not fit.
    #[inline(always)]
    pub fn append(&mut self, buf: &[u8]) {
        assert!(buf.len() <= self.remaining());
        // SAFETY: copying into self.buf with bounds checked above.
        unsafe {
            self.buf
                .as_mut_ptr()
                .add(*self.filled)
                .cast::<u8>()
                .copy_from_nonoverlapping(buf.as_ptr(), buf.len());
        }
        *self.filled += buf.len();
    }

    /// Extends the initialized portion of the buffer with `len` bytes equal to
    /// `val`. Panics if the data does not fit.
    #[inline(always)]
    pub fn fill(&mut self, val: u8, len: usize) {
        self.buf[*self.filled..][..len].fill(MaybeUninit::new(val));
        *self.filled += len;
    }

    /// Splits this buffer into two at `split_at` and calls `f` to fill out each
    /// part.
    ///
    /// If the left buffer is not filled in full but the right buffer is
    /// partially initialized, then the remainder of the left buffer will be
    /// zero-initialized.
    #[track_caller]
    pub fn write_split<R>(&mut self, split_at: usize, f: impl FnOnce(Buf<'_>, Buf<'_>) -> R) -> R {
        let (left, right) = self.buf[*self.filled..].split_at_mut(split_at);
        let mut left_filled = 0;
        let mut right_filled = 0;
        let r = f(
            Buf {
                buf: left,
                filled: &mut left_filled,
            },
            Buf {
                buf: right,
                filled: &mut right_filled,
            },
        );
        assert!(left_filled <= left.len());
        assert!(right_filled <= right.len());
        *self.filled += left_filled;
        if right_filled > 0 {
            let to_zero = left.len() - left_filled;
            self.fill(0, to_zero);
            *self.filled += right_filled;
        }
        r
    }
}

/// Calls `f` with a [`Buf`], which provides safe methods for
/// extending the initialized portion of the buffer.
pub fn write_with<T, F, R>(buffer: &mut T, f: F) -> R
where
    T: Buffer + ?Sized,
    F: FnOnce(Buf<'_>) -> R,
{
    let mut filled = 0;
    // SAFETY: Buf will only write initialized bytes to the buffer.
    let buf = unsafe { buffer.unwritten() };

    let r = f(Buf {
        buf,
        filled: &mut filled,
    });
    // SAFETY: `filled` bytes are known to have been written.
    unsafe {
        buffer.extend_written(filled);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::write_with;
    use alloc::vec;

    #[test]
    #[should_panic]
    fn test_append_vec_panic() {
        let mut v = vec![1, 2, 3];
        write_with(&mut v, |mut buf| {
            buf.append(&vec![0; buf.remaining() + 1]);
        });
    }

    #[test]
    fn test_append_vec() {
        let mut v = vec![1, 2, 3, 4];
        v.reserve(3);

        write_with(&mut v, |mut buf| {
            buf.append(&[5, 6]);
            buf.push(7);
        });
        assert_eq!(&v, &[1, 2, 3, 4, 5, 6, 7]);
    }
}
